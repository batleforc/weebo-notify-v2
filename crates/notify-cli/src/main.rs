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

use std::process::ExitCode;

use clap::{CommandFactory as _, Parser, Subcommand};
use notify_client_core::{ClientError, ConfigError, Profile, Session};

mod manage;
mod publish;
mod tui;

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
pub(crate) struct Cli {
    /// Server profile to talk to, letting one machine hold several servers
    /// (personal, work). Selects the config section and the local store.
    #[arg(long, global = true, env = "WNK_PROFILE", default_value = "default")]
    profile: String,

    /// Output format for one-shot commands.
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Table)]
    output: OutputFormat,

    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub(crate) enum OutputFormat {
    /// Human-readable columns.
    Table,
    /// Machine-readable JSON, stable across releases.
    Json,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
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

    /// Show the inbox, most recent first.
    Ls {
        /// Only this channel.
        #[arg(long, short = 'c', env = "WNK_CHANNEL")]
        channel: Option<String>,

        /// How many rows.
        #[arg(long, short = 'n', default_value_t = 20)]
        limit: u16,
    },

    /// Catch the local store up with everything since its cursor.
    Sync {
        /// Only this channel.
        #[arg(long, short = 'c', env = "WNK_CHANNEL")]
        channel: Option<String>,
    },

    /// Channels: list, create, join.
    Channel {
        #[command(subcommand)]
        command: manage::ChannelCommand,
    },

    /// Devices: list, and enrol this machine.
    Device {
        #[command(subcommand)]
        command: manage::DeviceCommand,
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

    match run(cli) {
        Ok(rendered) => {
            // Printed *here* and nowhere else. Every command returns what it has to
            // say instead of writing it out, which is what lets the TUI run the same
            // command and show the same answer in its popup (§9.2, §9.3).
            if !rendered.is_empty() {
                println!("{rendered}");
            }
            ExitCode::from(EXIT_OK)
        }
        Err(error) => {
            eprintln!("notify: {error}");
            ExitCode::from(error.exit_code())
        }
    }
}

fn run(cli: Cli) -> Result<String, CliError> {
    match cli.command {
        Some(Command::Completions { shell }) => Ok(completions(shell)),
        Some(Command::Watch) | None => tui::run(&cli.profile).map(|()| String::new()),
        // Only the commands that talk to a server pay for a runtime; `completions`
        // in a shell's startup path should not start tokio to print a script.
        Some(command) => on_runtime(|| async move {
            let session = Session::open(&Profile::load(&cli.profile)?)?;

            execute(&session, &cli.profile, cli.output, command).await
        }),
    }
}

/// Run one parsed command and give back what it has to say.
///
/// The single dispatch point, called by the shell above and by the TUI's `:` mode
/// (design §9.2). Two callers and one table is the mechanism §9 promises: a
/// subcommand added to [`Command`] is available in both without a line of TUI code —
/// and the match below stops compiling until it is handled, which is what keeps the
/// promise mechanical rather than remembered.
pub(crate) async fn execute(
    session: &Session,
    profile: &str,
    output: OutputFormat,
    command: Command,
) -> Result<String, CliError> {
    match command {
        Command::Publish {
            channel,
            idempotency_key,
            content,
        } => {
            publish::publish(
                session,
                &channel,
                content,
                idempotency_key.as_deref(),
                output,
            )
            .await
        }
        Command::Update {
            channel,
            id,
            content,
        } => publish::update(session, &channel, &id, content, output).await,
        Command::Ls { channel, limit } => {
            manage::list_inbox(session, channel.as_deref(), limit, output).await
        }
        Command::Sync { channel } => {
            manage::sync(session, profile, channel.as_deref(), output).await
        }
        Command::Channel { command } => manage::channel(session, command, output).await,
        Command::Device { command } => manage::device(session, command, profile, output).await,
        Command::Completions { shell } => Ok(completions(shell)),
        // Refused rather than ignored: from the shell this is handled before a
        // session is opened, and from inside the TUI it can only mean somebody typed
        // `:watch` in the window it would have opened.
        Command::Watch => Err(CliError::Usage(
            "`watch` opens the terminal UI, which is not something a command can do \
             from inside it"
                .to_owned(),
        )),
    }
}

/// The completion script for a shell, as a string.
fn completions(shell: clap_complete::Shell) -> String {
    let mut command = Cli::command();
    let name = command.get_name().to_string();
    let mut rendered = Vec::new();
    clap_complete::generate(shell, &mut command, name, &mut rendered);

    // Completion scripts are ASCII shell code; a generator that emitted anything
    // else is a bug worth seeing rather than one worth papering over.
    String::from_utf8_lossy(&rendered).into_owned()
}

/// Run one async command to completion.
fn on_runtime<F, T>(work: F) -> Result<String, CliError>
where
    F: FnOnce() -> T,
    T: Future<Output = Result<String, CliError>>,
{
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| CliError::Api(format!("could not start the async runtime: {error}")))?
        .block_on(work())
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
