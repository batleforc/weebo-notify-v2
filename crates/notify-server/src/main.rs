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
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::DumpOpenapi { output }) => dump_openapi(output.as_deref()),
        Some(Command::Doctor) => doctor_report(&cli.config),
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

#[actix_web::main]
async fn run(config_path: &std::path::Path) -> anyhow::Result<()> {
    // Read before the subscriber exists, since `[telemetry]` shapes it; failures go to stderr.
    let config = config::Config::load(config_path)?;
    let traces = telemetry::install(config.telemetry.as_ref())?;

    for pending in config.unhonoured() {
        // Parsed but not acted on: say so at startup.
        tracing::warn!(%pending, "configuration key not honoured by this build");
    }

    // The deployment-owed guarantees, once per boot (see `doctor`).
    doctor::log_review(&config);
    doctor::refuse_development_defaults(&config)?;

    let application = wiring::Application::build(&config).await?;

    // Migrate before serving.
    application.store.migrate().await?;

    // Under the reconcile lock, so replicas booting together do not deadlock.
    let election: std::sync::Arc<dyn notify_ports::LeaderElection> = std::sync::Arc::new(
        notify_persistence_pg::PgLeaderElection::new(application.store.pool().clone()),
    );
    let reconcile_owed = if let Some(guard) = lead_reconciliation(election.as_ref(), true).await {
        let outcome = application.reconciler.execute(&config).await;
        guard.release().await;
        let report = outcome?;
        application
            .reconciliation_log
            .record(report.as_pass(OffsetDateTime::now_utc(), notify_app::Trigger::Boot));
        tracing::info!(
            channels = report.channels,
            accounts = report.accounts,
            removed = report.removed,
            "applied the declarative configuration"
        );
        for pending in &report.pending {
            tracing::warn!(%pending, "access binding is waiting on a first login");
        }
        false
    } else {
        tracing::warn!("could not take the reconcile lock at boot; the timer will apply the file");
        true
    };

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
    let readiness = actix_web::web::Data::new(health::Dependencies::new(
        application.store.clone(),
        application.transports.names(),
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
                    .wrap(
                        actix_web::middleware::Logger::new(
                            r#"%a "%m %U" %s %b "%{Referer}i" "%{User-Agent}i" %T"#,
                        )
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
            app.service(utoipa_scalar::Scalar::with_url("/api/docs", api))
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
        application.metrics.clone(),
        Reconciling {
            reconciler: application.reconciler,
            log: application.reconciliation_log,
            config,
            path: config_path.to_path_buf(),
            policies: application.policies,
            election,
            owed: reconcile_owed,
        },
        store.clone(),
    ));

    tracing::info!(%bind, "weebo-notify is listening");
    let outcome = server.await;

    // Stop jobs while the runtime lives: dropping a sqlx connection after it aborts the process.
    // This also releases the leader lock immediately (§11).
    jobs.abort();
    let _ = jobs.await;
    pump.abort();
    let _ = pump.await;
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
}

impl Reconciling {
    /// Re-read the file and apply it, as §11 asks of `SIGHUP`.
    async fn on_hangup(&mut self) {
        match config::Config::load(&self.path) {
            Ok(file) => {
                // Named before anything is applied, so the operator learns it was ignored.
                for pending in self.config.unreloadable(&file) {
                    tracing::warn!(%pending, "changed in the file, not adopted by a reload");
                }
                self.config = self.config.reloaded_from(file);

                // Swapped before the pass, so the journal dates the change correctly (§13).
                match wiring::trust_policies(&self.config) {
                    Ok(policies) => self.policies.adopt(policies),
                    // The rest of the reload still stands.
                    Err(error) => tracing::warn!(
                        ?error,
                        "SIGHUP: the trust policies did not build; keeping the running set"
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
        self.pass(notify_app::Trigger::Sighup).await;
    }

    /// One reconciliation pass, recorded for the console (§10.4bis).
    /// A file is applied when read (boot, `SIGHUP`); timer passes only re-apply what the database
    /// last recorded, and a replica whose copy differs stands aside so stale pods cannot undo new ones.
    async fn pass(&mut self, trigger: notify_app::Trigger) {
        let full = self.owed || trigger != notify_app::Trigger::Timer;

        // One pass at a time across the deployment.
        let Some(guard) = lead_reconciliation(self.election.as_ref(), full).await else {
            return;
        };

        if !full {
            match self.reconciler.is_current(&self.config).await {
                Ok(true) => {}
                Ok(false) => {
                    tracing::debug!(
                        "another replica applied a different configuration; standing aside"
                    );
                    guard.release().await;
                    return;
                }
                Err(error) => {
                    tracing::warn!(?error, "could not compare the configuration; skipping");
                    guard.release().await;
                    return;
                }
            }
        }

        match self.reconciler.execute(&self.config).await {
            Ok(report) => {
                self.owed = false;
                // Recorded only on success, keeping the last good pass on the console.
                self.log
                    .record(report.as_pass(OffsetDateTime::now_utc(), trigger));
                tracing::info!(
                    channels = report.channels,
                    accounts = report.accounts,
                    removed = report.removed,
                    journalled = report.journalled,
                    ?trigger,
                    "reconcile pass complete"
                );
                for pending in &report.pending {
                    tracing::warn!(%pending, "access binding is waiting on a first login");
                }
            }
            Err(error) => tracing::warn!(?error, ?trigger, "reconcile pass failed"),
        }
        guard.release().await;
    }
}

/// Take the reconcile lock; a fresh-file pass waits (`wait`), a timer pass does not.
async fn lead_reconciliation(
    election: &dyn notify_ports::LeaderElection,
    wait: bool,
) -> Option<Box<dyn notify_ports::LeaderGuard>> {
    let attempts = if wait { 60 } else { 1 };
    for attempt in 0..attempts {
        if attempt > 0 {
            actix_web::rt::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        match election.try_acquire("reconcile").await {
            Ok(Some(guard)) => return Some(guard),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(?error, "could not ask for the reconcile lock");
                return None;
            }
        }
    }
    None
}

/// An interval that, after a stall, waits a full period rather than firing every
/// tick it missed back to back.
fn every(period: std::time::Duration) -> actix_web::rt::time::Interval {
    let mut interval = actix_web::rt::time::interval(period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval
}

/// The leader-tolerant background loops: TTL purge, stale closure, DND digests, sink delivery and
/// reconciliation. Separate concurrent loops, so a slow job never delays another's tick.
async fn background(
    purge: notify_app::PurgeExpired,
    close_stale: notify_app::CloseStale,
    digests: notify_app::SendDndDigests,
    drain: notify_app::DrainSinks,
    metrics: std::sync::Arc<dyn notify_ports::Metrics>,
    reconciling: Reconciling,
    store: notify_persistence_pg::PgStore,
) {
    let purging = async {
        let mut tick = every(JOB_INTERVAL);
        // The first tick fires immediately; every job just ran at boot.
        tick.tick().await;
        loop {
            tick.tick().await;
            match purge.execute().await {
                Ok(report) if report.led => {
                    tracing::info!(
                        expired = report.expired,
                        purged = report.purged,
                        events_pruned = report.events_pruned,
                        "purge pass complete"
                    );
                    // Rides on the purge's leadership.
                    if let Err(error) = store.prune_rate_limits(OffsetDateTime::now_utc()).await {
                        tracing::warn!(?error, "could not prune rate-limit counters");
                    }
                }
                // Not leading is normal on all but one replica.
                Ok(_) => {}
                Err(error) => tracing::warn!(?error, "purge pass failed; retrying next tick"),
            }
        }
    };

    // Independent of the purge (separate lock); what this closes the next purge can expire (§7.2bis).
    let closing = async {
        let mut tick = every(JOB_INTERVAL);
        tick.tick().await;
        loop {
            tick.tick().await;
            match close_stale.execute().await {
                Ok(report) if report.led => tracing::info!(
                    closed = report.closed,
                    channels_watched = report.channels_watched,
                    "stale-closure pass complete"
                ),
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(?error, "stale-closure pass failed; retrying next tick");
                }
            }
        }
    };

    // A quiet period ending is the absence of an event, so poll (§7.4).
    let digesting = async {
        let mut tick = every(JOB_INTERVAL);
        tick.tick().await;
        loop {
            tick.tick().await;
            match digests.execute().await {
                Ok(report) if report.sent > 0 => tracing::info!(
                    sent = report.sent,
                    still_quiet = report.still_quiet,
                    "do-not-disturb digests sent"
                ),
                Ok(_) => {}
                Err(error) => tracing::warn!(?error, "digest pass failed; retrying next tick"),
            }
        }
    };

    // The sinks of §7.1bis; logged only when something moved.
    let draining = async {
        let mut tick = every(SINK_INTERVAL);
        tick.tick().await;
        loop {
            tick.tick().await;
            match drain.execute().await {
                Ok(report) => {
                    if report.delivered + report.retried + report.buried + report.orphaned > 0 {
                        tracing::info!(
                            delivered = report.delivered,
                            retried = report.retried,
                            buried = report.buried,
                            orphaned = report.orphaned,
                            "sink drain pass complete"
                        );
                    }

                    // Leader-only (§13): followers leave the series absent rather than claim zero.
                    if report.led {
                        match drain.dead_letter_depth().await {
                            Ok(depths) => metrics.dlq_depth(&depths),
                            Err(error) => {
                                tracing::warn!(?error, "could not read the dead-letter depth");
                            }
                        }
                    }
                }
                Err(error) => tracing::warn!(?error, "sink drain failed; retrying next tick"),
            }
        }
    };

    tokio::join!(
        purging,
        closing,
        digesting,
        draining,
        reconcile_loop(reconciling)
    );
}

/// Reconciliation on the timer and on `SIGHUP`, in one loop so passes never interleave.
async fn reconcile_loop(mut reconciling: Reconciling) {
    let mut tick = every(RECONCILE_INTERVAL);
    tick.tick().await;

    // Without a SIGHUP handler we keep running, but report it.
    let mut hangups = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
        Ok(stream) => Some(stream),
        Err(error) => {
            tracing::warn!(?error, "cannot listen for SIGHUP; reload will not work");
            None
        }
    };

    loop {
        tokio::select! {
            // Disabled outright, or `None` would make this branch spin.
            Some(()) = async {
                match hangups.as_mut() {
                    Some(stream) => stream.recv().await,
                    None => std::future::pending().await,
                }
            } => reconciling.on_hangup().await,
            _ = tick.tick() => reconciling.pass(notify_app::Trigger::Timer).await,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing assertion is a failing test")]

    use actix_web::{App, HttpResponse, test, web};

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
