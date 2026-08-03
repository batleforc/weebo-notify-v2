//! `notify publish` and `notify update` — the two ingestion paths, at a prompt.
//!
//! Thin by rule (design §9.3): everything here is argument parsing and rendering.
//! Anything that decides something belongs in `notify-client-core`, because the TUI
//! and the Tauri shell will want it too.

use std::io::Read as _;

use clap::{Args, ValueEnum};
use notify_client_core::types::{Priority, Published, State, Status, Ttl};
use notify_client_core::{Revision, Session};

use crate::{CliError, OutputFormat};

/// The fields both commands share.
#[derive(Debug, Args)]
pub(crate) struct Content {
    /// Headline. Required when creating; leave it out on an update to keep the one
    /// that is there.
    #[arg(long, short = 't')]
    pub title: Option<String>,

    /// Markdown body. `-` reads it from stdin, which is what makes
    /// `cmd | notify publish -c ci-infra -t "…" -b -` work.
    #[arg(long, short = 'b')]
    pub body: Option<String>,

    /// Lifecycle position. Supplying it at creation is what makes the notification
    /// a lifecycle one rather than fire-and-forget (design §2.2).
    #[arg(long, value_enum)]
    pub state: Option<StateArg>,

    /// Outcome.
    #[arg(long, value_enum)]
    pub status: Option<StatusArg>,

    /// How loudly.
    #[arg(long, value_enum)]
    pub priority: Option<PriorityArg>,

    /// Retention, e.g. `48h`, `7d`, or `0` for forever where the channel allows it.
    #[arg(long)]
    pub ttl: Option<String>,

    /// A tag. Repeat the flag for several.
    #[arg(long = "tag")]
    pub tags: Vec<String>,
}

impl Content {
    /// Turn the flags into a revision, reading stdin if asked to.
    pub(crate) fn into_revision(self) -> Result<Revision, CliError> {
        let body = match self.body.as_deref() {
            Some("-") => Some(read_stdin()?),
            other => other.map(ToOwned::to_owned),
        };

        Ok(Revision {
            title: self.title,
            body,
            state: self.state.map(Into::into),
            status: self.status.map(Into::into),
            priority: self.priority.map(Into::into),
            ttl: self.ttl.map(Ttl::from),
            tags: self.tags,
            ..Revision::default()
        })
    }
}

fn read_stdin() -> Result<String, CliError> {
    let mut buffer = String::new();
    std::io::stdin()
        .read_to_string(&mut buffer)
        .map_err(|error| CliError::Usage(format!("could not read the body from stdin: {error}")))?;
    Ok(buffer)
}

/// Create a notification.
pub(crate) async fn publish(
    session: &Session,
    channel: &str,
    content: Content,
    idempotency_key: Option<&str>,
    output: OutputFormat,
) -> Result<String, CliError> {
    let published = session
        .publish(channel, content.into_revision()?, idempotency_key)
        .await?;

    render(&published, output)
}

/// Create or revise by the publisher's own identifier.
pub(crate) async fn update(
    session: &Session,
    channel: &str,
    external_id: &str,
    content: Content,
    output: OutputFormat,
) -> Result<String, CliError> {
    let published = session
        .update(channel, external_id, content.into_revision()?)
        .await?;

    render(&published, output)
}

fn render(published: &Published, output: OutputFormat) -> Result<String, CliError> {
    match output {
        OutputFormat::Json => {
            // Whole response, verbatim: a script that wants a field should not have
            // to hope the CLI happened to print it.
            serde_json::to_string_pretty(published)
                .map_err(|error| CliError::Api(format!("could not render the response: {error}")))
        }
        OutputFormat::Table => {
            let mut out = format!(
                "{}\n  channel   {}\n  state     {:?} / {:?}\n  revision  {}\n  cursor    {}",
                published.notification.id,
                published.notification.channel,
                published.revision.state,
                published.revision.status,
                published.revision.seq,
                published.cursor,
            );

            // A clamp the publisher cannot see is a clamp they will report as a bug
            // (design §7.2bis, scenario T6).
            if let Some(requested) = &published.retention.clamped_from {
                use std::fmt::Write as _;

                let _ = write!(
                    out,
                    "\n  ttl       {} — clamped from the {} you asked for",
                    published.retention.ttl,
                    // `Deref<Target = String>` is progenitor's rendering of a
                    // string newtype; there is no `Display` to reach for.
                    **requested
                );
            }
            if published.replayed {
                out.push_str(
                    "\n  replayed  yes — this is the stored answer to an earlier identical call",
                );
            }

            Ok(out)
        }
    }
}

/// The constrained values, as clap sees them.
///
/// One variant per server-side value, and the mapping below is exhaustive: adding a
/// state to the domain fails to compile here until the CLI learns it, which is the
/// only way the two stay in step (design §9.1).
macro_rules! value_enum {
    ($name:ident => $target:path { $($variant:ident),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, ValueEnum)]
        #[value(rename_all = "snake_case")]
        pub(crate) enum $name {
            $($variant),+
        }

        impl From<$name> for $target {
            fn from(value: $name) -> Self {
                match value {
                    $($name::$variant => <$target>::$variant),+
                }
            }
        }
    };
}

value_enum!(StateArg => State { Open, Working, Closed });
value_enum!(StatusArg => Status { Success, Failure, Warning, Info, Cancelled });
value_enum!(PriorityArg => Priority { Min, Low, Default, High, Urgent });
