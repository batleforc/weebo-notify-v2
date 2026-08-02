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

mod health;
mod openapi;

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

fn run(config: &std::path::Path) -> anyhow::Result<()> {
    anyhow::bail!(
        "the server is not wired yet (milestone M1.8); \
         would have started from {}",
        config.display()
    )
}
