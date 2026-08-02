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
enum OutputFormat {
    /// Human-readable columns.
    Table,
    /// Machine-readable JSON, stable across releases.
    Json,
}

#[derive(Debug, Subcommand)]
enum Command {
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

    let result = match cli.command {
        Some(Command::Completions { shell }) => {
            let mut command = Cli::command();
            let name = command.get_name().to_string();
            clap_complete::generate(shell, &mut command, name, &mut std::io::stdout());
            Ok(())
        }
        Some(Command::Watch) | None => open_tui(),
    };

    match result {
        Ok(()) => ExitCode::from(EXIT_OK),
        Err(error) => {
            eprintln!("notify: {error}");
            ExitCode::from(error.exit_code())
        }
    }
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
enum CliError {
    /// The server said no, or could not be reached.
    Api(String),
    /// The command line itself does not make sense here.
    Usage(String),
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
