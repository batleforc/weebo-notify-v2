//! The `notify` binary (design §9).
//!
//! One executable, two modes: scriptable one-shot commands (clap) and, from
//! milestone M2, a full-screen real-time client (ratatui). The TUI is the default
//! mode — running `notify` with no subcommand opens it — while any subcommand
//! short-circuits straight to one-shot mode.
//!
//! Parity between the two is not maintained by hand: the TUI's `:` command mode
//! feeds the line it is given to *this same* clap parser, so a new subcommand is
//! available in the TUI by construction (design §9.2).

use std::io::IsTerminal as _;
use std::process::ExitCode;

use clap::{CommandFactory as _, Parser, Subcommand};
use notify_client_core::{ClientError, ConfigError, Profile, Session};

mod publish;

/// Exit codes are part of the CLI contract so scripts can branch on them
/// without parsing output (design §9.1).
const EXIT_OK: u8 = 0;
const EXIT_API_ERROR: u8 = 1;
const EXIT_USAGE: u8 = 2;

#[derive(Debug, Parser)]
#[command(
    name = "notify",
    version,
    about = "weebo-notify client — publish, follow and acknowledge notifications",
    long_about = "Run without a subcommand to open the terminal UI.\n\
                  Any subcommand runs one-shot and exits."
)]
struct Cli {
    /// Server profile to talk to, letting one machine hold several servers
    /// (personal, work). Selects the config section and the local store.
    #[arg(long, global = true, env = "WNK_PROFILE", default_value = "default")]
    profile: String,

    /// Output format for one-shot commands.
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Table)]
    output: OutputFormat,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub(crate) enum OutputFormat {
    /// Human-readable columns.
    Table,
    /// Machine-readable JSON, stable across releases.
    Json,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Publish a notification.
    Publish {
        /// Which channel it goes to.
        #[arg(long, short = 'c', env = "WNK_CHANNEL")]
        channel: String,

        /// Replayed for 24 hours: sending the same key twice publishes once.
        /// Exactly what a CI job that may be retried wants (design §8).
        #[arg(long)]
        idempotency_key: Option<String>,

        #[command(flatten)]
        content: publish::Content,
    },

    /// Create or revise a notification by your own identifier.
    ///
    /// The idempotent path: the same call creates on the first run and revises on
    /// the retry, so a pipeline never has to ask which one it is doing.
    Update {
        /// Which channel it lives in.
        #[arg(long, short = 'c', env = "WNK_CHANNEL")]
        channel: String,

        /// Your identifier for it — a CI run id, a job name, an incident reference.
        #[arg(long, short = 'i')]
        id: String,

        #[command(flatten)]
        content: publish::Content,
    },

    /// Open the terminal UI. Explicit alias of running `notify` with no subcommand.
    Watch,

    /// Generate shell completions for bash, zsh, fish or PowerShell.
    Completions {
        /// Shell to generate completions for.
        shell: clap_complete::Shell,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let result = run(cli);

    match result {
        Ok(()) => ExitCode::from(EXIT_OK),
        Err(error) => {
            eprintln!("notify: {error}");
            ExitCode::from(error.exit_code())
        }
    }
}

fn run(cli: Cli) -> Result<(), CliError> {
    match cli.command {
        Some(Command::Completions { shell }) => {
            let mut command = Cli::command();
            let name = command.get_name().to_string();
            clap_complete::generate(shell, &mut command, name, &mut std::io::stdout());
            Ok(())
        }
        Some(Command::Watch) | None => open_tui(),
        // Only the commands that talk to a server pay for a runtime; `completions`
        // in a shell's startup path should not start tokio to print a script.
        Some(command) => on_runtime(|| async move {
            let session = Session::open(&Profile::load(&cli.profile)?)?;

            match command {
                Command::Publish {
                    channel,
                    idempotency_key,
                    content,
                } => {
                    publish::publish(
                        &session,
                        &channel,
                        content,
                        idempotency_key.as_deref(),
                        cli.output,
                    )
                    .await
                }
                Command::Update {
                    channel,
                    id,
                    content,
                } => publish::update(&session, &channel, &id, content, cli.output).await,
                Command::Watch | Command::Completions { .. } => unreachable(),
            }
        }),
    }
}

/// Run one async command to completion.
fn on_runtime<F, T>(work: F) -> Result<(), CliError>
where
    F: FnOnce() -> T,
    T: Future<Output = Result<(), CliError>>,
{
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| CliError::Api(format!("could not start the async runtime: {error}")))?
        .block_on(work())
}

/// The two variants handled above cannot reach here; saying so beats a `panic!`,
/// which is denied workspace-wide for good reason.
fn unreachable() -> Result<(), CliError> {
    Err(CliError::Usage("unsupported command".to_owned()))
}

fn open_tui() -> Result<(), CliError> {
    // Non-interactive guard rail (design §9.1): in a pipe, a cron job or CI, the
    // TUI would block forever. Print the help and fail with the usage code instead.
    if !std::io::stdout().is_terminal() {
        let _ = Cli::command().print_help();
        return Err(CliError::Usage(
            "stdout is not a terminal — refusing to open the TUI".to_owned(),
        ));
    }

    Err(CliError::Api(
        "the terminal UI lands in milestone M2".to_owned(),
    ))
}

#[derive(Debug)]
pub(crate) enum CliError {
    /// The server said no, or could not be reached.
    Api(String),
    /// The command line itself does not make sense here.
    Usage(String),
}

impl From<ClientError> for CliError {
    fn from(error: ClientError) -> Self {
        // The stable code goes in the message rather than being swallowed: a script
        // that greps for `field.empty` should find it on stderr (design §9.1).
        Self::Api(error.to_string())
    }
}

impl From<ConfigError> for CliError {
    fn from(error: ConfigError) -> Self {
        // Configuration is part of the invocation, so a bad profile exits 2 with
        // the usage codes rather than looking like the server refused something.
        Self::Usage(error.to_string())
    }
}

impl CliError {
    fn exit_code(&self) -> u8 {
        match self {
            Self::Api(_) => EXIT_API_ERROR,
            Self::Usage(_) => EXIT_USAGE,
        }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Api(message) | Self::Usage(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for CliError {}
