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
use utoipa_actix_web::AppExt as _;
use utoipa_scalar::Servable as _;

mod bus;
mod config;
mod health;
mod openapi;
mod push;
mod reconcile;
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
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::DumpOpenapi { output }) => dump_openapi(output.as_deref()),
        None => run(&cli.config),
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

#[actix_web::main]
async fn run(config_path: &std::path::Path) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,sqlx=warn")),
        )
        .init();

    let config = config::Config::load(config_path)?;
    for pending in config.unhonoured() {
        // Parsed and not acted on. Saying so at startup is the difference between
        // "this build does not do that yet" and an afternoon of debugging.
        tracing::warn!(%pending, "configuration key not honoured by this build");
    }

    let application = wiring::Application::build(&config).await?;

    // Migrations before anything is served: a replica that answers requests against
    // a schema it has not finished applying is worse than one that is slow to start.
    application.store.migrate().await?;

    let report = application.reconciler.execute(&config).await?;
    tracing::info!(
        channels = report.channels,
        accounts = report.accounts,
        removed = report.removed,
        "applied the declarative configuration"
    );
    for pending in &report.pending {
        tracing::warn!(%pending, "access binding is waiting on a first login");
    }

    let bind = config.server.bind;
    let cors_origins = config.server.cors_origins.clone();
    let state = actix_web::web::Data::new(application.state);
    let readiness = actix_web::web::Data::new(health::Dependencies::new(
        application.store.clone(),
        application.transports.names(),
    ));

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
            .map(|app| {
                app.wrap(cors)
                    .wrap(actix_web::middleware::Logger::default().log_target("http"))
                    .configure(health::configure)
            })
            .configure(notify_http_actix::configure)
            .split_for_parts();

        app.service(utoipa_scalar::Scalar::with_url("/api/docs", api))
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
        application.reconciler,
        config,
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

    outcome.map_err(Into::into)
}

/// The background loops: TTL purge and reconciliation, both leader-tolerant.
async fn background(
    purge: notify_app::PurgeExpired,
    reconciler: reconcile::Reconciler,
    config: config::Config,
) {
    let mut purge_tick = actix_web::rt::time::interval(JOB_INTERVAL);
    let mut reconcile_tick = actix_web::rt::time::interval(RECONCILE_INTERVAL);
    // The first tick fires immediately and both have just run at boot.
    purge_tick.tick().await;
    reconcile_tick.tick().await;

    loop {
        tokio::select! {
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
            _ = reconcile_tick.tick() => match reconciler.execute(&config).await {
                Ok(report) => for pending in &report.pending {
                    tracing::warn!(%pending, "access binding is waiting on a first login");
                },
                Err(error) => tracing::warn!(?error, "reconcile pass failed; retrying next tick"),
            },
        }
    }
}
