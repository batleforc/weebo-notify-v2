//! `oidc-mock` — the development identity provider (design §12.1).
//!
//! Runs as a plain process next to the server and the web dev server, not as a
//! container: that is what lets one binary serve Compose, Eclipse Che and CI
//! without an image to pull, a sidecar to schedule or a registry to depend on.

use std::path::PathBuf;

use actix_web::{App, HttpServer, web};
use clap::Parser;
use notify_oidc_mock::{Identities, Mock, configure};

#[derive(Debug, Parser)]
#[command(
    name = "oidc-mock",
    version,
    about = "Development OIDC provider for weebo-notify — never for production"
)]
struct Cli {
    /// The identities to serve.
    #[arg(
        long,
        default_value = "dev/oidc-mock/identities.toml",
        env = "WNK_OIDC_MOCK_IDENTITIES"
    )]
    identities: PathBuf,

    /// Where to listen.
    #[arg(long, default_value = "0.0.0.0:9110", env = "WNK_OIDC_MOCK_BIND")]
    bind: String,

    /// What the provider calls itself, overriding the identities file.
    ///
    /// This is how Eclipse Che points the mock at the workspace's public route:
    /// OIDC redirects travel through the developer's browser, which cannot reach
    /// the pod's `localhost` (§12.1).
    #[arg(long, env = "WNK_OIDC_MOCK_ISSUER")]
    issuer: Option<String>,
}

#[actix_web::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    let mut identities = Identities::load(&cli.identities)?;
    if let Some(issuer) = cli.issuer {
        identities = identities.with_issuer(issuer);
    }

    let issuer = identities.issuer.clone();
    let accounts: Vec<_> = identities
        .users
        .iter()
        .map(|user| user.username.clone())
        .collect();

    let mock = web::Data::new(Mock::new(identities).map_err(|error| anyhow::anyhow!(error))?);

    tracing::info!(
        %issuer,
        bind = %cli.bind,
        accounts = %accounts.join(", "),
        "development OIDC provider ready — do not deploy this anywhere real"
    );

    HttpServer::new(move || App::new().app_data(mock.clone()).configure(configure))
        .bind(&cli.bind)?
        .run()
        .await?;

    Ok(())
}
