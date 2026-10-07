//! The `notify` binary (§9): one-shot clap commands, or the TUI when run without a subcommand.
//! The TUI's `:` mode feeds the same clap parser, so every subcommand works there too (§9.2).

use std::process::ExitCode;

use clap::{CommandFactory as _, Parser, Subcommand};
use notify_client_core::{ClientError, ConfigError, Profile, Session};

mod login;
mod manage;
mod publish;
mod tui;

/// Exit codes are part of the CLI contract (§9.1).
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
    /// Server profile to talk to (selects the config section and the local store).
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

        /// Replayed for 24 hours: sending the same key twice publishes once (§8).
        #[arg(long)]
        idempotency_key: Option<String>,

        #[command(flatten)]
        content: publish::Content,
    },

    /// Create or revise a notification by your own identifier (idempotent: safe to retry).
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

    /// Do not disturb: quiet hours and the quick toggle (§7.4).
    Dnd {
        #[command(subcommand)]
        command: manage::DndCommand,
    },

    /// Devices: list, and enrol this machine.
    Device {
        #[command(subcommand)]
        command: manage::DeviceCommand,
    },

    /// Tokens: the CI federation exchange (design §5.3).
    Token {
        #[command(subcommand)]
        command: TokenCommand,
    },

    /// Mint a personal API key in the browser and save it here (§9.1).
    ///
    /// Opens (or prints, under `NO_BROWSER`) a link to the web client, where the key is minted.
    Login {
        /// Where the web client lives (a different origin than the API server).
        #[arg(long, env = "WNK_WEB_URL", default_value = "http://localhost:5173")]
        web_url: String,
    },

    /// Open the terminal UI. Explicit alias of running `notify` with no subcommand.
    Watch {
        /// Also relay arrivals to the desktop (§9.2); do-not-disturb (§7.4) is honoured either way.
        #[arg(long)]
        notify_send: bool,
    },

    /// Generate shell completions for bash, zsh, fish or PowerShell.
    Completions {
        /// Shell to generate completions for.
        shell: clap_complete::Shell,
    },
}

/// The federation exchange (design §5.3, §9.1).
#[derive(Debug, Subcommand)]
pub(crate) enum TokenCommand {
    /// Trade a forge ID token for a short-lived weebo-notify token (§5.3), printed alone.
    Exchange {
        /// The forge ID token; prefer the environment (an argument shows in `ps`).
        #[arg(long, env = "WNK_CI_TOKEN", hide_env_values = true)]
        token: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match run(cli) {
        Ok(rendered) => {
            // Printed only here: commands return their output so the TUI can show it too (§9.2).
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
        Some(Command::Watch { notify_send }) => {
            tui::run(&cli.profile, notify_send).map(|()| String::new())
        }
        // Interactive and needs no credential, so dispatched before a `Session` exists.
        Some(Command::Login { web_url }) => on_runtime(|| login::run(&cli.profile, &web_url)),
        None => tui::run(&cli.profile, false).map(|()| String::new()),
        // Only server commands start tokio; `completions` shouldn't.
        Some(command) => on_runtime(|| async move {
            let session = Session::open(&Profile::load(&cli.profile)?)?;

            execute(&session, &cli.profile, cli.output, command).await
        }),
    }
}

/// Run one parsed command and give back what it has to say. The single dispatch point for
/// the shell and the TUI's `:` mode (§9.2), so the match keeps both in step.
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
        Command::Dnd { command } => manage::dnd(session, command, output).await,
        Command::Token {
            command: TokenCommand::Exchange { token },
        } => {
            let minted = session.exchange(&token).await?;
            // The token alone, for `T=$(notify token exchange)`.
            Ok(minted.token)
        }
        Command::Completions { shell } => Ok(completions(shell)),
        // From the shell this is handled earlier; from the TUI it means `:watch` inside the TUI.
        Command::Watch { .. } => Err(CliError::Usage(
            "`watch` opens the terminal UI, which is not something a command can do \
             from inside it"
                .to_owned(),
        )),
        // As `watch`; also `login` reads stdin, which the TUI's raw mode owns.
        Command::Login { .. } => Err(CliError::Usage(
            "`login` ne peut pas s'exécuter depuis le TUI — lance `notify login` dans \
             un autre terminal"
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
        // The stable code stays in the message so scripts can grep stderr (§9.1).
        Self::Api(error.to_string())
    }
}

impl From<ConfigError> for CliError {
    fn from(error: ConfigError) -> Self {
        // A bad profile is a usage error (exit 2), not a server refusal.
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use super::*;

    #[test]
    fn the_command_tree_is_well_formed() {
        // Catches clap misconfiguration here rather than on first use.
        Cli::command().debug_assert();
    }

    #[test]
    fn both_spellings_of_enrol_reach_the_same_command() {
        // §9.1 writes `enroll`; the variant is `enrol`.
        for spelling in ["enrol", "enroll"] {
            let cli = Cli::try_parse_from(["notify", "device", spelling]).unwrap();

            assert!(
                matches!(
                    cli.command,
                    Some(Command::Device {
                        command: manage::DeviceCommand::Enrol { .. }
                    })
                ),
                "`{spelling}` did not reach the enrolment command"
            );
        }
    }

    #[test]
    fn every_command_can_be_asked_for_json() {
        // §9.1 promises `--output json|table` on every command.
        for command in Cli::command().get_subcommands() {
            let name = command.get_name();
            assert!(
                command
                    .get_arguments()
                    .chain(
                        // Global args show on subcommands only after propagation; check both.
                        Cli::command().get_arguments()
                    )
                    .any(|argument| argument.get_long() == Some("output")),
                "`notify {name}` cannot be asked for JSON (§9.1)"
            );
        }
    }
}
