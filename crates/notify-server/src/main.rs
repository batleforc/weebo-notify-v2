//! weebo-notify server binary: the composition root (design §4).
//! Config layers: defaults < TOML file < `WNK_*` env. `dump-openapi` exports the spec with no infra (§3.2).

use std::path::PathBuf;

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use time::OffsetDateTime;
use utoipa_actix_web::AppExt as _;
use utoipa_scalar::Servable as _;

mod bus;
mod config;
mod doctor;
mod health;
mod openapi;
mod push;
mod reconcile;
mod sinks;
mod telemetry;
mod wiring;

#[derive(Debug, Parser)]
#[command(
    name = "notify-server",
    version,
    about = "weebo-notify — rich, multi-channel, multi-device notification server"
)]
struct Cli {
    /// Path to the TOML configuration file. Every key it holds can also be set
    /// through a `WNK_`-prefixed environment variable (design §11).
    #[arg(
        long,
        short,
        global = true,
        env = "WNK_CONFIG",
        default_value = "config/dev.toml"
    )]
    config: PathBuf,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Write the OpenAPI document to a file (or stdout) and exit.
    DumpOpenapi {
        /// Destination file. Defaults to stdout.
        #[arg(long, short)]
        output: Option<PathBuf>,
    },

    /// Review this deployment's configuration against the hardening checklist, print what it finds,
    /// and exit non-zero on anything that looks wrong. Connects to nothing; safe to paste into an issue.
    Doctor,

    /// Apply the database migrations and exit.
    ///
    /// For deployments that set `[database] migrate_on_start = false` and run the
    /// schema change once, as a Job or an init container, rather than from every
    /// replica as it boots. Exits 0 once the schema is current.
    Migrate,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::DumpOpenapi { output }) => dump_openapi(output.as_deref()),
        Some(Command::Doctor) => doctor_report(&cli.config),
        Some(Command::Migrate) => migrate(&cli.config),
        None => run(&cli.config),
    }
}

/// `notify-server doctor`.
fn doctor_report(config_path: &std::path::Path) -> anyhow::Result<()> {
    let config = config::Config::load(config_path)?;
    if doctor::report(&config) {
        Ok(())
    } else {
        // Non-zero, so it can gate a deployment pipeline.
        std::process::exit(1);
    }
}

/// `notify-server migrate`.
#[actix_web::main]
async fn migrate(config_path: &std::path::Path) -> anyhow::Result<()> {
    let config = config::Config::load(config_path)?;
    let traces = telemetry::install(config.telemetry.as_ref())?;

    let store = notify_persistence_pg::PgStore::connect_with(
        config.database.url.expose(),
        &config.database.pool(),
    )
    .await?;
    let outcome = store.migrate().await;
    // Closed while the runtime is alive, for the reason `run` gives at shutdown.
    store.pool().close().await;
    outcome?;

    tracing::info!("the database schema is current");
    traces.shutdown();
    Ok(())
}

/// Migrate at boot, or check that somebody already did (`[database]
/// migrate_on_start`).
///
/// Refusing is the point of the second half: a replica serving against a schema
/// older than its code fails in ways that look like data bugs — a missing column
/// read as an empty one — and the one clear moment to say so is before it binds.
async fn prepare_schema(
    store: &notify_persistence_pg::PgStore,
    database: &config::Database,
) -> anyhow::Result<()> {
    if database.migrate_on_start {
        // Migrations before anything is served: a replica that answers requests
        // against a schema it has not finished applying is worse than one that is
        // slow to start.
        store.migrate().await?;
        return Ok(());
    }

    let pending = store.pending_migrations().await?;
    anyhow::ensure!(
        pending.is_empty(),
        "the database schema is behind this build ({} migration(s) not applied, first {:?}) \
         and [database] migrate_on_start = false; run `notify-server migrate` first",
        pending.len(),
        pending.first()
    );
    Ok(())
}

fn dump_openapi(output: Option<&std::path::Path>) -> anyhow::Result<()> {
    let document = openapi::document();
    // Pretty-printed with a trailing newline so the committed file diffs cleanly.
    let mut rendered =
        serde_json::to_string_pretty(&document).context("serialising the OpenAPI document")?;
    rendered.push('\n');

    match output {
        Some(path) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            std::fs::write(path, rendered)
                .with_context(|| format!("writing {}", path.display()))?;
        }
        None => print!("{rendered}"),
    }

    Ok(())
}

/// How often the background jobs wake; costs one advisory-lock attempt per replica.
const JOB_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// How often the reconciler re-applies the configuration, so `email`/`oidc_sub` bindings resolve
/// after that person's first login without waiting for a restart or `SIGHUP`.
const RECONCILE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// How often the leader looks for due sink deliveries (design §7.1bis): this is first-attempt latency.
const SINK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// How often the fan-out outbox worker looks without being woken (ADR-0025).
///
/// A publish on this replica wakes it at once; the tick is for everything else —
/// rows another replica committed and did not get to, and rows whose worker died
/// and whose lease has run out.
const FANOUT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

#[actix_web::main]
async fn run(config_path: &std::path::Path) -> anyhow::Result<()> {
    // First: SIGHUP's default action kills the process, so a reload during a slow boot would.
    // The stream queues the signal until the reconcile loop reads it.
    let hangups = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup());

    // Read before the subscriber exists, since `[telemetry]` shapes it; failures go to stderr.
    let config = config::Config::load(config_path)?;
    let traces = telemetry::install(config.telemetry.as_ref())?;

    // Without a SIGHUP handler we keep running, but report it.
    let hangups = match hangups {
        Ok(stream) => Some(stream),
        Err(error) => {
            tracing::warn!(?error, "cannot listen for SIGHUP; reload will not work");
            None
        }
    };

    for pending in config.unhonoured() {
        // Parsed but not acted on: say so at startup.
        tracing::warn!(%pending, "configuration key not honoured by this build");
    }

    // The deployment-owed guarantees, once per boot (see `doctor`).
    doctor::log_review(&config);
    doctor::refuse_development_defaults(&config)?;

    let application = wiring::Application::build(&config).await?;
    prepare_schema(&application.store, &config.database).await?;

    // Before the drain claims anything, so no row waits under an old sink id. Idempotent.
    let rekeyed = sinks::rekey_stored(&config, &application.routes, &application.store).await?;
    if rekeyed > 0 {
        tracing::info!(
            rows = rekeyed,
            "stored sink ids moved to the current fingerprint key"
        );
    }

    // Under the reconcile lock, so replicas booting together do not deadlock. The boot pass runs
    // in the reconcile loop after the listener binds (a lock wait must not trip liveness);
    // `/readyz` reports `startup` until it has run.
    let election: std::sync::Arc<dyn notify_ports::LeaderElection> = std::sync::Arc::new(
        notify_persistence_pg::PgLeaderElection::new(application.store.pool().clone()),
    );
    let started = health::Started::default();

    if application.routes.len() > 0 {
        // Counts and kinds only: URLs key the sink secrets.
        tracing::info!(
            sinks = application.routes.len(),
            kinds = ?application.routes.kinds(),
            "outbound sinks configured"
        );
    }

    let bind = config.server.bind;
    let cors_origins = config.server.cors_origins.clone();
    let serve_docs = config.server.docs;
    let state = actix_web::web::Data::new(application.state);
    let streams = std::sync::Arc::clone(&state.streams);
    let bus = state.bus.clone();
    let readiness = actix_web::web::Data::new(health::Dependencies::new(
        application.store.clone(),
        application.transports.names(),
        application.bus.clone(),
        started.clone(),
    ));
    // One registry shared with the HTTP layer and drain loop, which `/metrics` renders.
    let exporter = actix_web::web::Data::new(application.metrics.clone());

    let server = actix_web::HttpServer::new(move || {
        // Closed by default (§5.1); there are no cookies to protect.
        let mut cors = actix_cors::Cors::default()
            .allow_any_method()
            .allow_any_header()
            .expose_any_header()
            .max_age(3_600);
        for origin in &cors_origins {
            cors = cors.allowed_origin(origin);
        }

        // Routes register with their OpenAPI paths, so the docs are the route table (§3.2).
        let (app, api) = actix_web::App::new()
            .into_utoipa_app()
            .openapi(openapi::base())
            .app_data(state.clone())
            .app_data(readiness.clone())
            .app_data(exporter.clone())
            .map(|app| {
                app.wrap(cors)
                    .wrap(security_headers())
                    // Default format without the query string: `?token=` is a bearer credential.
                    // actix has no `%m`, hence the custom replacement for the method.
                    .wrap(
                        actix_web::middleware::Logger::new(
                            r#"%a "%{METHOD}xi %U" %s %b "%{Referer}i" "%{User-Agent}i" %T"#,
                        )
                        .custom_request_replace("METHOD", |req| req.method().to_string())
                        .log_target("http"),
                    )
                    // Outermost, so the access log line lands inside the request span (§13).
                    .wrap(actix_web::middleware::from_fn(telemetry::continue_trace))
                    .configure(health::configure)
            })
            .configure(notify_http_actix::configure)
            .split_for_parts();

        // Unmounted rather than `401`, which would confirm it exists.
        if serve_docs {
            app.service(
                utoipa_scalar::Scalar::with_url("/api/docs", api)
                    .custom_html(include_str!("scalar.html")),
            )
        } else {
            app
        }
    })
    .bind(bind)?
    // Long enough for an in-flight publish; SSE connections never finish by themselves.
    .shutdown_timeout(15)
    .run();

    let pump = application.pump;
    let store = application.store;
    let jobs = actix_web::rt::spawn(background(
        application.purge,
        application.close_stale,
        application.digests,
        application.drain,
        application.fanout_drain,
        application.metrics.clone(),
        Reconciling {
            reconciler: application.reconciler,
            log: application.reconciliation_log,
            config,
            path: config_path.to_path_buf(),
            policies: application.policies,
            election,
            owed: true,
            routes: application.routes,
            fleet: application.fleet,
            started,
            metrics: application.metrics.clone(),
        },
        hangups,
        store.clone(),
        bus,
    ));
    let leases = actix_web::rt::spawn(renew_leases(streams));

    tracing::info!(%bind, "weebo-notify is listening");
    let outcome = server.await;

    // Stop jobs while the runtime lives: dropping a sqlx connection after it aborts the process.
    // This also releases the leader lock immediately (§11).
    jobs.abort();
    leases.abort();
    // Aborted on purpose: the outcome is a cancellation, and shutdown goes on either way.
    if let Err(error) = jobs.await
        && !error.is_cancelled()
    {
        tracing::error!(%error, "the background jobs ended abnormally");
    }
    pump.abort();
    if let Err(error) = pump.await
        && !error.is_cancelled()
    {
        tracing::error!(%error, "the poke pump ended abnormally");
    }
    store.pool().close().await;

    tracing::info!("stopped");

    // Last, so everything above is flushed.
    traces.shutdown();

    outcome.map_err(Into::into)
}

/// Headers every response carries unless its route set its own: no sniffing, no
/// framing, no caching of authorised JSON, and no `?token=` leaking via `Referer`.
fn security_headers() -> actix_web::middleware::DefaultHeaders {
    actix_web::middleware::DefaultHeaders::new()
        .add(("x-content-type-options", "nosniff"))
        .add(("x-frame-options", "DENY"))
        .add(("referrer-policy", "no-referrer"))
        .add(("cache-control", "no-store"))
}

/// Everything the reconciliation half of [`background`] needs (design §11).
struct Reconciling {
    reconciler: reconcile::Reconciler,
    log: notify_app::ReconciliationLog,
    config: config::Config,
    /// The path, re-read on `SIGHUP`.
    path: std::path::PathBuf,
    /// The set the token exchange consults, swapped on reload (§5.3).
    policies: notify_app::LivePolicies,
    /// Who may run a pass right now: one replica at a time.
    election: std::sync::Arc<dyn notify_ports::LeaderElection>,
    /// Set when this replica read its file but has not applied it yet, so the next timer pass is full.
    owed: bool,
    /// The sink routing table and its adapters, swapped on `SIGHUP`.
    routes: sinks::SinkRoutes,
    fleet: sinks::SinkFleet,
    /// Set once the configuration has been applied (or found already applied by
    /// someone else), which is when `/readyz` stops reporting `startup`.
    started: health::Started,
    /// Where each pass's outcome is counted.
    metrics: std::sync::Arc<dyn notify_ports::Metrics>,
}

impl Reconciling {
    /// Re-read the file and apply it, as §11 asks of `SIGHUP`.
    async fn on_hangup(&mut self) -> bool {
        match config::Config::load(&self.path) {
            Ok(file) => {
                let pending = self.config.unreloadable(&file);
                let candidate = self.config.reloaded_from(file);
                // Built before the swap: the pass journals the file's policies, so adopting a
                // file whose policies are not enforced would audit a removal that never took
                // effect. All of the reload, or none of it.
                let policies = match wiring::trust_policies(&candidate) {
                    Ok(policies) => policies,
                    Err(error) => {
                        tracing::warn!(
                            ?error,
                            "SIGHUP: the trust policies did not build; keeping the running configuration"
                        );
                        // As for a file that does not load: the running one is re-applied.
                        self.owed = true;
                        return self.pass(notify_app::Trigger::Sighup).await;
                    }
                };
                // Named before anything is applied, so the operator learns it was ignored.
                for pending in pending {
                    tracing::warn!(%pending, "changed in the file, not adopted by a reload");
                }
                self.config = candidate;
                // Swapped before the pass, so the journal dates the change correctly (§13).
                self.policies.adopt(policies);

                // The sinks, on the same terms: all of them or none, and a sink
                // that does not build (a `secret_ref` nobody mounted) keeps the
                // running set rather than dropping a working webhook over it. The
                // adapters go first, so no new route ever resolves to a kind with
                // no adapter behind it.
                match sinks::SinkRoutes::build(&self.config).and_then(|routes| {
                    let fleet = wiring::sink_fleet(&self.config, &routes)?;
                    Ok((routes, fleet))
                }) {
                    Ok((routes, fleet)) => {
                        self.fleet.adopt(&fleet);
                        self.routes.adopt(&routes);
                        tracing::info!(
                            sinks = self.routes.len(),
                            kinds = ?self.routes.kinds(),
                            "SIGHUP: sinks re-read"
                        );
                    }
                    Err(error) => tracing::warn!(
                        ?error,
                        "SIGHUP: the sinks did not build; keeping the running set"
                    ),
                }

                tracing::info!("configuration re-read on SIGHUP");
            }
            // The last good configuration keeps running: a typo must not take a replica down.
            Err(error) => tracing::warn!(
                ?error,
                "SIGHUP: the configuration did not load; keeping the running one"
            ),
        }

        self.owed = true;
        self.pass(notify_app::Trigger::Sighup).await
    }

    /// One reconciliation pass, recorded for the console (§10.4bis).
    /// A file is applied when read (boot, `SIGHUP`); timer passes only re-apply what the database
    /// last recorded, and a replica whose copy differs stands aside so stale pods cannot undo new ones.
    /// Returns whether nothing failed (standing aside is not a failure), for the job metrics.
    async fn pass(&mut self, trigger: notify_app::Trigger) -> bool {
        let full = self.owed || trigger != notify_app::Trigger::Timer;

        // One pass at a time across the deployment.
        let Ok(led) = lead_reconciliation(self.election.as_ref(), full).await else {
            // The database did not answer: not "somebody else leads". `/readyz` stays at
            // `startup`, and the next tick retries the whole file (still owed).
            return false;
        };
        let Some(guard) = led else {
            if trigger == notify_app::Trigger::Boot {
                // Somebody else held it for the whole wait — a long pass on another
                // replica. Serving is still right: the database is in whatever
                // state that pass leaves, and the timer applies this file next.
                tracing::warn!(
                    "could not take the reconcile lock at boot; the timer will apply the file"
                );
                self.started.finish();
            }
            return true;
        };

        if !full {
            match self.reconciler.is_current(&self.config).await {
                Ok(true) => {}
                Ok(false) => {
                    tracing::debug!(
                        "another replica applied a different configuration; standing aside"
                    );
                    guard.release().await;
                    return true;
                }
                Err(error) => {
                    tracing::warn!(?error, "could not compare the configuration; skipping");
                    guard.release().await;
                    return false;
                }
            }
        }

        let ok = match self.reconciler.execute(&self.config).await {
            Ok(report) => {
                // Refused removals stay in the snapshot, so `is_current` answers false on
                // every replica: still owed, or the timer would stand aside until a SIGHUP.
                self.owed = report.refused > 0;
                self.started.finish();
                // Recorded only on success, keeping the last good pass on the console.
                self.log
                    .record(report.as_pass(OffsetDateTime::now_utc(), trigger));
                tracing::info!(
                    channels = report.channels,
                    accounts = report.accounts,
                    removed = report.removed,
                    refused = report.refused,
                    journalled = report.journalled,
                    ?trigger,
                    "reconcile pass complete"
                );
                for pending in &report.pending {
                    tracing::warn!(%pending, "access binding is waiting on a first login");
                }
                true
            }
            // At boot this keeps `/readyz` reporting `startup`: a file that does
            // not apply is a replica that should not take traffic, and the timer
            // retries it in full (`owed` is still set).
            Err(error) => {
                tracing::error!(?error, ?trigger, "reconcile pass failed");
                false
            }
        };
        guard.release().await;
        ok
    }
}

/// Take the reconcile lock; a fresh-file pass waits (`wait`), a timer pass does not.
async fn lead_reconciliation(
    election: &dyn notify_ports::LeaderElection,
    wait: bool,
) -> Result<Option<Box<dyn notify_ports::LeaderGuard>>, ()> {
    let attempts = if wait { 60 } else { 1 };
    for attempt in 0..attempts {
        if attempt > 0 {
            actix_web::rt::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        match election.try_acquire("reconcile").await {
            Ok(Some(guard)) => return Ok(Some(guard)),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(?error, "could not ask for the reconcile lock");
                return Err(());
            }
        }
    }
    Ok(None)
}

/// An interval that, after a stall, waits a full period rather than firing every
/// tick it missed back to back.
fn every(period: std::time::Duration) -> actix_web::rt::time::Interval {
    let mut interval = actix_web::rt::time::interval(period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval
}

/// The leader-tolerant background loops: TTL purge, stale closure, DND digests, sink delivery, the
/// fan-out outbox and reconciliation. Separate concurrent loops, so a slow job never delays another's
/// tick, each [`supervise`]d so an error or panic is counted and retried rather than ending it.
#[expect(
    clippy::too_many_arguments,
    reason = "one per job, and each is a different type; a struct would only rename them"
)]
async fn background(
    purge: notify_app::PurgeExpired,
    close_stale: notify_app::CloseStale,
    digests: notify_app::SendDndDigests,
    drain: notify_app::DrainSinks,
    fanout_drain: notify_app::DrainFanout,
    metrics: std::sync::Arc<dyn notify_ports::Metrics>,
    reconciling: Reconciling,
    hangups: Option<tokio::signal::unix::Signal>,
    store: notify_persistence_pg::PgStore,
    bus: std::sync::Arc<dyn notify_ports::EventBus>,
) {
    let credential_window = reconciling
        .config
        .retention
        .revoked_credentials
        .as_duration();
    let (purge, close_stale, digests, drain, store) =
        (&purge, &close_stale, &digests, &drain, &store);
    let metrics = metrics.as_ref();

    let purging = supervise("purge", JOB_INTERVAL, metrics, move || async move {
        let report = purge.execute().await?;
        // Not leading is the normal answer on all but one replica.
        if report.led {
            tracing::info!(
                expired = report.expired,
                purged = report.purged,
                events_pruned = report.events_pruned,
                audit_purged = report.audit_purged,
                "purge pass complete"
            );
            // Rides on the purge's leadership.
            if let Err(error) = store.prune_rate_limits(OffsetDateTime::now_utc()).await {
                tracing::warn!(?error, "could not prune rate-limit counters");
            }
            housekeep(store, metrics, credential_window).await;
        }
        Ok::<_, notify_ports::Failure>(())
    });

    // Independent of the purge (separate lock); what this closes the next purge can expire (§7.2bis).
    let closing = supervise("close-stale", JOB_INTERVAL, metrics, move || async move {
        let report = close_stale.execute().await?;
        if report.led {
            tracing::info!(
                closed = report.closed,
                channels_watched = report.channels_watched,
                "stale-closure pass complete"
            );
        }
        Ok::<_, notify_ports::Failure>(())
    });

    // A quiet period ending is the absence of an event, so poll (§7.4).
    let digesting = supervise("dnd-digest", JOB_INTERVAL, metrics, move || async move {
        let report = digests.execute().await?;
        if report.sent > 0 {
            tracing::info!(
                sent = report.sent,
                still_quiet = report.still_quiet,
                "do-not-disturb digests sent"
            );
        }
        Ok::<_, notify_ports::Failure>(())
    });

    // The sinks of §7.1bis; logged only when something moved.
    let draining = supervise("sink-drain", SINK_INTERVAL, metrics, move || async move {
        let report = drain.execute().await?;
        if report.moved() {
            tracing::info!(
                delivered = report.delivered,
                retried = report.retried,
                buried = report.buried,
                orphaned = report.orphaned,
                pruned = report.pruned,
                "sink drain pass complete"
            );
        }

        // Leader-only (§13): followers leave the series absent rather than claim zero.
        if report.led {
            match drain.dead_letter_depth().await {
                Ok(depths) => metrics.dlq_depth(&depths),
                Err(error) => tracing::warn!(?error, "could not read the dead-letter depth"),
            }
        }
        Ok::<_, notify_ports::Failure>(())
    });

    // Device pushes and thumbnails of committed publishes (ADR-0025), on every replica (rows are
    // leased, not led). Woken by a local publish, so its own loop rather than `supervise`'s tick.
    let fanning_out = async {
        let mut tick = every(FANOUT_INTERVAL);
        let mut failures = 0_u32;
        loop {
            let ok = guarded("fan-out", async {
                fanout_drain.execute().await.map(|_| ())
            })
            .await;
            metrics.job_finished("fan-out", ok);
            failures = if ok { 0 } else { failures.saturating_add(1) };
            if failures > 0 {
                actix_web::rt::time::sleep(backoff(failures)).await;
            }
            tokio::select! {
                _ = tick.tick() => {}
                () = fanout_drain.woken() => {}
            }
        }
    };

    tokio::join!(
        purging,
        closing,
        digesting,
        draining,
        fanning_out,
        // Every replica, like the fan-out: it wakes this replica's own streams
        // when a long transaction stops holding events back (ADR-0028).
        crate::bus::watch_watermark(store, bus.as_ref(), metrics),
        reconcile_loop(reconciling, hangups)
    );
}

/// Every replica keeps its own stream leases alive and closes the ones their owner
/// disconnected from another replica (§8.2).
async fn renew_leases(streams: std::sync::Arc<notify_http_actix::StreamSlots>) {
    let mut tick = every(notify_persistence_pg::LEASE_RENEW);
    loop {
        tick.tick().await;
        if let Err(error) = streams.renew().await {
            tracing::warn!(?error, "could not renew stream leases");
        }
    }
}

/// Run `pass` every `period` for ever, surviving errors and panics, with [`backoff`] after failures.
/// Panics are caught in place, not in a spawned child, which would outlive the shutdown abort.
/// Every outcome is counted on `metrics`.
async fn supervise<F, Fut, E>(
    job: &'static str,
    period: std::time::Duration,
    metrics: &dyn notify_ports::Metrics,
    mut pass: F,
) where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), E>>,
    E: core::fmt::Debug,
{
    let mut tick = every(period);
    // The first tick fires immediately; skip it so jobs do not race the boot.
    tick.tick().await;
    let mut failures = 0_u32;

    loop {
        tick.tick().await;
        let ok = guarded(job, pass()).await;
        metrics.job_finished(job, ok);

        if ok {
            failures = 0;
        } else {
            failures = failures.saturating_add(1);
            actix_web::rt::time::sleep(backoff(failures)).await;
        }
    }
}

/// How long one background pass may run before it is dropped. Each stage has its own
/// budget well under this; a pass still running here is stuck (a query on a connection
/// that died silently), and dropping it also drops its leader lock.
const PASS_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(5);

/// One pass, its error, panic or hang logged and turned into `false`.
async fn guarded<Fut, E>(job: &'static str, pass: Fut) -> bool
where
    Fut: std::future::Future<Output = Result<(), E>>,
    E: core::fmt::Debug,
{
    guarded_within(job, PASS_TIMEOUT, pass).await
}

/// [`guarded`] with its own limit, so a test need not wait five minutes.
async fn guarded_within<Fut, E>(job: &'static str, limit: std::time::Duration, pass: Fut) -> bool
where
    Fut: std::future::Future<Output = Result<(), E>>,
    E: core::fmt::Debug,
{
    use futures::FutureExt as _;

    let Ok(outcome) =
        actix_web::rt::time::timeout(limit, std::panic::AssertUnwindSafe(pass).catch_unwind())
            .await
    else {
        tracing::error!(job, ?limit, "background pass stuck; dropped it");
        return false;
    };
    match outcome {
        Ok(Ok(())) => true,
        Ok(Err(error)) => {
            tracing::warn!(job, ?error, "background pass failed; retrying");
            false
        }
        Err(panic) => {
            let message = panic
                .downcast_ref::<&str>()
                .map(|text| (*text).to_owned())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_default();
            tracing::error!(job, %message, "background pass panicked; restarting the loop");
            false
        }
    }
}

/// The extra wait after `failures` consecutive failed passes: 5 s doubling, capped
/// at five minutes.
fn backoff(failures: u32) -> std::time::Duration {
    let doublings = failures.saturating_sub(1).min(6);
    std::time::Duration::from_secs(5 << doublings).min(std::time::Duration::from_secs(300))
}

/// The age past which a fan-out row still owed is logged as a drain falling behind.
const FANOUT_STALE_AFTER_SECONDS: u64 = 5 * 60;

/// Bookkeeping on the purge's leadership (design §7.2bis, §13): spent federation token ids,
/// long-revoked credentials, and backlog gauges. Each step logs and carries on.
async fn housekeep(
    store: &notify_persistence_pg::PgStore,
    metrics: &dyn notify_ports::Metrics,
    credential_window: std::time::Duration,
) {
    let now = OffsetDateTime::now_utc();

    // An id past its token's expiry guards nothing.
    match notify_ports::ReplayGuard::prune(store, now).await {
        Ok(0) => {}
        Ok(pruned) => tracing::info!(pruned, "spent federation token ids pruned"),
        Err(error) => tracing::warn!(?error, "could not prune federation replay ids"),
    }

    // A window past what `time` represents keeps every revoked credential, which is safe.
    let window = time::Duration::try_from(credential_window).unwrap_or(time::Duration::MAX);
    let before = now.saturating_sub(window);
    match store.prune_revoked_credentials(before).await {
        Ok((0, 0)) => {}
        Ok((devices, keys)) => {
            tracing::info!(devices, keys, "long-revoked credentials deleted");
        }
        Err(error) => tracing::warn!(?error, "could not prune revoked credentials"),
    }

    match store
        .retention_backlog(
            now,
            now.saturating_sub(notify_app::EVENT_RETENTION),
            now.saturating_sub(notify_app::TOMBSTONE_GRACE),
        )
        .await
    {
        Ok(backlog) => metrics.retention_backlog(&backlog),
        Err(error) => tracing::warn!(?error, "could not measure the retention backlog"),
    }

    match store.purge_silences(now, 1_000).await {
        Ok(0) => {}
        Ok(purged) => tracing::info!(purged, "silences that can no longer apply deleted"),
        Err(error) => tracing::warn!(?error, "could not purge silences"),
    }

    // Dead letters are kept as long as events, for an operator to read; bounded per pass.
    match store
        .purge_dead_fanout(now.saturating_sub(notify_app::EVENT_RETENTION), 1_000)
        .await
    {
        Ok(0) => {}
        Ok(purged) => tracing::info!(purged, "old dead-lettered fan-out rows deleted"),
        Err(error) => tracing::warn!(?error, "could not purge dead-lettered fan-out rows"),
    }

    match store.fanout_backlog(now).await {
        Ok(backlog) => {
            metrics.fanout_backlog(&backlog);
            // Rows past an hour are retired as dead, so this old means the drain is behind or down.
            if backlog.oldest_pending_age_seconds > FANOUT_STALE_AFTER_SECONDS {
                tracing::warn!(
                    pending = backlog.pending,
                    age_seconds = backlog.oldest_pending_age_seconds,
                    "fan-out outbox falling behind"
                );
            }
            if backlog.dead > 0 {
                tracing::warn!(dead = backlog.dead, "fan-out rows dead-lettered");
            }
        }
        Err(error) => tracing::warn!(?error, "could not measure the fan-out outbox"),
    }
}

/// Reconciliation at boot, on the timer and on `SIGHUP`, in one loop so passes never interleave;
/// each pass is [`guarded`], so a panic does not end reloads.
async fn reconcile_loop(
    mut reconciling: Reconciling,
    mut hangups: Option<tokio::signal::unix::Signal>,
) {
    use futures::FutureExt as _;

    // The boot pass; `/readyz` holds traffic back until it has run.
    let boot = std::panic::AssertUnwindSafe(reconciling.pass(notify_app::Trigger::Boot))
        .catch_unwind()
        .await
        .unwrap_or(false);
    reconciling.metrics.job_finished("reconcile", boot);

    let mut tick = every(RECONCILE_INTERVAL);
    tick.tick().await;

    loop {
        let metrics = std::sync::Arc::clone(&reconciling.metrics);
        let ok = tokio::select! {
            // Disabled outright, or `None` would make this branch spin.
            Some(()) = async {
                match hangups.as_mut() {
                    Some(stream) => stream.recv().await,
                    None => std::future::pending().await,
                }
            } => std::panic::AssertUnwindSafe(reconciling.on_hangup()).catch_unwind().await,
            _ = tick.tick() => {
                std::panic::AssertUnwindSafe(reconciling.pass(notify_app::Trigger::Timer))
                    .catch_unwind()
                    .await
            }
        };
        let ok = ok.unwrap_or_else(|_| {
            tracing::error!("reconcile pass panicked; the loop carries on");
            false
        });
        metrics.job_finished("reconcile", ok);
    }
}

#[cfg(test)]
mod tests {
    use actix_web::{App, HttpResponse, test, web};

    #[actix_web::test]
    async fn a_stuck_pass_is_dropped_and_counted_as_failed() {
        let stuck = std::future::pending::<Result<(), ()>>();

        let ok = super::guarded_within("test", std::time::Duration::from_millis(50), stuck).await;

        assert!(
            !ok,
            "a hung pass must end, or its loop and leader lock hang with it"
        );
    }

    #[actix_web::test]
    async fn responses_carry_the_security_headers_without_overriding_a_route() {
        let app = test::init_service(
            App::new()
                .wrap(super::security_headers())
                .route("/json", web::get().to(HttpResponse::Ok))
                .route(
                    "/bytes",
                    web::get().to(|| async {
                        HttpResponse::Ok()
                            .insert_header(("cache-control", "private, max-age=60"))
                            .finish()
                    }),
                ),
        )
        .await;

        let json =
            test::call_service(&app, test::TestRequest::get().uri("/json").to_request()).await;
        for (name, value) in [
            ("x-content-type-options", "nosniff"),
            ("x-frame-options", "DENY"),
            ("referrer-policy", "no-referrer"),
            ("cache-control", "no-store"),
        ] {
            assert_eq!(json.headers().get(name).unwrap(), value, "{name}");
        }

        let bytes =
            test::call_service(&app, test::TestRequest::get().uri("/bytes").to_request()).await;
        assert_eq!(
            bytes.headers().get("cache-control").unwrap(),
            "private, max-age=60"
        );
    }
}
