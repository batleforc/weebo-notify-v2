//! weebo-notify server binary — the composition root (design §4).
//!
//! Everything the server does is wired here: configuration is layered
//! (defaults < TOML file < `WNK_*` environment), adapters are constructed once,
//! and the use cases of [`notify_app`] are handed the ports they need.
//!
//! The binary is deliberately more than a `run` command: `dump-openapi` lets
//! `task recu` export the OpenAPI document without booting a server, database or
//! object store, which is what keeps the `contract` CI job cheap (design §3.2).

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
    ///
    /// The output is committed to the repository, so the generated Rust and
    /// TypeScript clients can never drift from the server (design §3.2).
    DumpOpenapi {
        /// Destination file. Defaults to stdout.
        #[arg(long, short)]
        output: Option<PathBuf>,
    },

    /// Review this deployment's configuration against the hardening checklist,
    /// print what it finds, and exit non-zero on anything that looks wrong.
    ///
    /// Reads configuration only — it connects to nothing, so it is safe to run
    /// against a live deployment and its output is safe to paste into an issue.
    /// The guarantees it *cannot* check are listed in the output rather than
    /// quietly omitted.
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
        // Non-zero, so this is usable in a deployment pipeline as a gate rather
        // than only as something a human reads.
        std::process::exit(1);
    }
}

fn dump_openapi(output: Option<&std::path::Path>) -> anyhow::Result<()> {
    let document = openapi::document();
    // Pretty-printed with a trailing newline so the committed file is diff-friendly
    // and the `contract` job only ever reports real API changes.
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

/// How often the background jobs wake.
///
/// The purge is leader-only and re-checks leadership between batches, so a short
/// interval costs one advisory-lock attempt per replica per minute and nothing else.
const JOB_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// How often the reconciler re-applies the configuration.
///
/// At boot and on `SIGHUP` is what design §11 asks for. The timer is here for one
/// case those two miss: a `[[channels.managed.access]]` binding by `email` or
/// `oidc_sub` cannot become a membership until that person has logged in once, so
/// without it they would wait for the next restart. Group bindings need none of
/// this — they resolve from the token.
const RECONCILE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// How often the leader looks for sink deliveries that are due (design §7.1bis).
///
/// Shorter than the others, and that is the whole difference between "a webhook
/// arrives" and "a webhook arrives eventually": a publish queues rather than sends
/// (`notify_app::sinks`), so this interval *is* the delivery latency for the first
/// attempt. Five seconds costs one advisory-lock attempt per replica per five
/// seconds and buys a consumer that sees an incident while it is still an incident.
const SINK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

#[actix_web::main]
async fn run(config_path: &std::path::Path) -> anyhow::Result<()> {
    // Read before the subscriber exists, because `[telemetry]` decides what the
    // subscriber is. The window is deliberately this short: a failure in here is
    // reported by `main` on stderr, and everything after it is logged properly.
    let config = config::Config::load(config_path)?;
    let traces = telemetry::install(config.telemetry.as_ref())?;

    for pending in config.unhonoured() {
        // Parsed and not acted on. Saying so at startup is the difference between
        // "this build does not do that yet" and an afternoon of debugging.
        tracing::warn!(%pending, "configuration key not honoured by this build");
    }

    // The deployment-owed guarantees, said out loud once per boot. Every item in
    // `hardening.mdx` fails silently — nothing logs "your bucket is world-readable"
    // — so an operator who never reads the documentation still meets the list here.
    // `notify-server doctor` prints the same review on demand.
    doctor::log_review(&config);

    let application = wiring::Application::build(&config).await?;

    // Migrations before anything is served: a replica that answers requests against
    // a schema it has not finished applying is worse than one that is slow to start.
    application.store.migrate().await?;

    let report = application.reconciler.execute(&config).await?;
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

    if application.routes.len() > 0 {
        // Counts and kinds, never destinations: a webhook url is where the sink's
        // secret is keyed, and a startup log is the least private file on the host.
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
    // The same registry the HTTP layer and the drain loop write to — `/metrics`
    // renders it, and a second one here would export an empty document while
    // everything counted into the first.
    let exporter = actix_web::web::Data::new(application.metrics.clone());

    let server = actix_web::HttpServer::new(move || {
        // Closed by default (§5.1): an origin that is not listed cannot call the
        // API from a browser, and there is no cookie to protect because there is no
        // session at all.
        let mut cors = actix_cors::Cors::default()
            .allow_any_method()
            .allow_any_header()
            .expose_any_header()
            .max_age(3_600);
        for origin in &cors_origins {
            cors = cors.allowed_origin(origin);
        }

        // The routes register themselves *and* their OpenAPI paths in one pass, so
        // the document Scalar serves is the route table rather than a description
        // of it (design §3.2 — the `contract` job diffs the same document).
        let (app, api) = actix_web::App::new()
            .into_utoipa_app()
            .openapi(openapi::base())
            .app_data(state.clone())
            .app_data(readiness.clone())
            .app_data(exporter.clone())
            .map(|app| {
                app.wrap(cors)
                    .wrap(actix_web::middleware::Logger::default().log_target("http"))
                    // Outermost of the two, so the access log line lands inside the
                    // request span rather than beside it (design §13).
                    .wrap(actix_web::middleware::from_fn(telemetry::continue_trace))
                    .configure(health::configure)
            })
            .configure(notify_http_actix::configure)
            .split_for_parts();

        // `[server] docs = false` unmounts it entirely rather than answering `401`:
        // a route that exists and refuses still confirms it is there.
        if serve_docs {
            app.service(utoipa_scalar::Scalar::with_url("/api/docs", api))
        } else {
            app
        }
    })
    .bind(bind)?
    // A drain that outlives an in-flight publish, and no longer: SSE connections
    // are long-lived by design, so waiting for *them* would never finish.
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
        },
    ));

    tracing::info!(%bind, "weebo-notify is listening");
    let outcome = server.await;

    // Everything below happens while the runtime is still alive, and that is the
    // whole point. A sqlx connection dropped after it has gone panics in its
    // destructor, and a panic in a destructor aborts the process — so a `SIGTERM`
    // would end in a core dump rather than a clean exit.
    //
    // Stopping the jobs first also releases the leader lock immediately, rather
    // than leaving the instance leaderless until the connection is recycled
    // (design §11).
    jobs.abort();
    let _ = jobs.await;
    pump.abort();
    let _ = pump.await;
    store.pool().close().await;

    tracing::info!("stopped");

    // Last, so the line above is in the batch: the exporter holds finished spans
    // until its next tick, and a process that exits without flushing drops exactly
    // the spans of whatever made somebody restart it.
    traces.shutdown();

    outcome.map_err(Into::into)
}

/// The background loops: TTL purge, do-not-disturb digests and reconciliation, all
/// leader-tolerant.
/// Everything the reconciliation half of [`background`] needs (design §11).
///
/// One struct rather than four parameters, and it earns its place: these four move
/// together — the file is re-read on `SIGHUP`, applied by the reconciler, and left
/// on the log for the console. Passing them separately made the signature longer
/// than the loop it configures.
struct Reconciling {
    reconciler: reconcile::Reconciler,
    log: notify_app::ReconciliationLog,
    config: config::Config,
    /// The path and not the contents, because the point of the signal is that the
    /// file has changed since it was last read.
    path: std::path::PathBuf,
    /// The set the token exchange consults, swapped on reload (§5.3, D31).
    policies: notify_app::LivePolicies,
}

impl Reconciling {
    /// Re-read the file and apply it, as §11 asks of `SIGHUP`.
    async fn on_hangup(&mut self) {
        match config::Config::load(&self.path) {
            Ok(file) => {
                // Named before anything is applied: an operator who edited
                // `[database]` and sent a signal has to learn that here, and not
                // from the absence of an effect.
                for pending in self.config.unreloadable(&file) {
                    tracing::warn!(%pending, "changed in the file, not adopted by a reload");
                }
                self.config = self.config.reloaded_from(file);

                // Swapped before the pass, not after: the pass journals what the
                // trust policies became (§13, D28), and an entry written before the
                // process adopted them would date the change to the wrong hour.
                match wiring::trust_policies(&self.config) {
                    Ok(policies) => self.policies.adopt(policies),
                    // The rest of the reload still stands. A malformed pattern is
                    // one section being wrong, and refusing the channels because of
                    // it would make one typo undo an unrelated edit.
                    Err(error) => tracing::warn!(
                        ?error,
                        "SIGHUP: the trust policies did not build; keeping the running set"
                    ),
                }

                tracing::info!("configuration re-read on SIGHUP");
            }
            // The last good configuration keeps running. A reload is not a restart,
            // and a typo must not take a serving replica down — least of all at the
            // moment somebody is editing the file.
            Err(error) => tracing::warn!(
                ?error,
                "SIGHUP: the configuration did not load; keeping the running one"
            ),
        }

        self.pass(notify_app::Trigger::Sighup).await;
    }

    /// One reconciliation pass, recorded for the console (§10.4bis).
    async fn pass(&self, trigger: notify_app::Trigger) {
        match self.reconciler.execute(&self.config).await {
            Ok(report) => {
                // Recorded only on success: a failed pass must leave the last good
                // one on the console, not replace it with nothing.
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
    }
}

async fn background(
    purge: notify_app::PurgeExpired,
    close_stale: notify_app::CloseStale,
    digests: notify_app::SendDndDigests,
    drain: notify_app::DrainSinks,
    metrics: std::sync::Arc<dyn notify_ports::Metrics>,
    mut reconciling: Reconciling,
) {
    let mut purge_tick = actix_web::rt::time::interval(JOB_INTERVAL);
    let mut closure_tick = actix_web::rt::time::interval(JOB_INTERVAL);
    let mut digest_tick = actix_web::rt::time::interval(JOB_INTERVAL);
    let mut sink_tick = actix_web::rt::time::interval(SINK_INTERVAL);
    let mut reconcile_tick = actix_web::rt::time::interval(RECONCILE_INTERVAL);
    // The first tick fires immediately and all of these have just run at boot.
    purge_tick.tick().await;
    closure_tick.tick().await;
    digest_tick.tick().await;
    sink_tick.tick().await;
    reconcile_tick.tick().await;

    // A process that cannot listen for the signal keeps running: reconciliation
    // still happens at boot and on the timer, so the failure is "no SIGHUP" and
    // not "no server". It is reported rather than swallowed, because an operator
    // sending a signal into a process that never registered for it would get
    // silence and no way to tell why.
    let mut hangups = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
        Ok(stream) => Some(stream),
        Err(error) => {
            tracing::warn!(?error, "cannot listen for SIGHUP; reload will not work");
            None
        }
    };

    loop {
        tokio::select! {
            // `Option::None` would make this branch fire in a loop, so it is
            // disabled outright when the handler could not be installed.
            Some(()) = async {
                match hangups.as_mut() {
                    Some(stream) => stream.recv().await,
                    None => std::future::pending().await,
                }
            } => reconciling.on_hangup().await,
            _ = purge_tick.tick() => match purge.execute().await {
                Ok(report) if report.led => tracing::info!(
                    expired = report.expired,
                    purged = report.purged,
                    events_pruned = report.events_pruned,
                    "purge pass complete"
                ),
                // Not leading is the normal answer on all but one replica.
                Ok(_) => {}
                Err(error) => tracing::warn!(?error, "purge pass failed; retrying next tick"),
            },
            // Its own arm on the same interval rather than a step inside the purge:
            // the two are independent jobs with independent leader locks, and a
            // purge that fails must not stop notifications from being closed. What
            // this closes becomes what the *next* purge tick can expire — a
            // lifecycle notification does not age until it closes (§7.2bis).
            _ = closure_tick.tick() => match close_stale.execute().await {
                Ok(report) if report.led => tracing::info!(
                    closed = report.closed,
                    channels_watched = report.channels_watched,
                    "stale-closure pass complete"
                ),
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(?error, "stale-closure pass failed; retrying next tick");
                }
            },
            // A quiet period ending is the absence of an event, so somebody has to
            // look — which is why this is a timer rather than a moment (§7.4).
            _ = digest_tick.tick() => match digests.execute().await {
                Ok(report) if report.sent > 0 => tracing::info!(
                    sent = report.sent,
                    still_quiet = report.still_quiet,
                    "do-not-disturb digests sent"
                ),
                Ok(_) => {}
                Err(error) => tracing::warn!(?error, "digest pass failed; retrying next tick"),
            },
            // The sinks of §7.1bis. Logged only when something moved: a quiet
            // instance ticks twelve times a minute, and a line each would bury
            // everything else in the log.
            _ = sink_tick.tick() => match drain.execute().await {
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

                    // `sink_dlq_depth` is refreshed by the leader and by nobody else
                    // (§13): a follower publishing zero would claim nothing is stuck
                    // on an instance where something is. The series is simply absent
                    // on the replicas that do not know.
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
            },
            _ = reconcile_tick.tick() => reconciling.pass(notify_app::Trigger::Timer).await,
        }
    }
}
