//! The commands that are not publishing: channels, devices, the inbox (design §9.1).
//!
//! # Both output formats, everywhere
//!
//! Every command here answers to `--output json|table`, and that is a contract
//! rather than a convenience (§9.1): the table is for a person, the JSON is what a
//! CI step parses instead of grepping columns that get realigned in the next
//! release.
//!
//! # Nothing here decides anything
//!
//! These are renderings of `notify-client-core` calls. Any rule that appeared in
//! this file — who may join a channel, what a default TTL is — would have to be
//! written again in the TUI and a third time in the Tauri shell (§9.3).
//!
//! # Every command *returns* its output rather than printing it
//!
//! That is §9.3 applied to presentation, and it is what makes the TUI's `:` mode
//! able to run these commands rather than only parse them: the shell prints what
//! comes back, the TUI puts the very same string in its popup. A `println!` here
//! would be output the TUI cannot reach — and a second rendering of every command
//! is exactly what §9 says not to write.

use notify_client_core::types::{ChannelPolicyBody, Ttl};
use notify_client_core::{Session, Store};

use crate::{CliError, OutputFormat};

/// `notify channel …`
#[derive(Debug, clap::Subcommand)]
pub(crate) enum ChannelCommand {
    /// List the channels you can see.
    List,

    /// Create a channel. You become its first manager.
    Create {
        /// Its id, and the name it is known by everywhere.
        id: String,

        /// A human label. Falls back to the id when absent.
        #[arg(long)]
        display_name: Option<String>,
    },

    /// Join a channel you are allowed to join.
    Join {
        /// Which one.
        id: String,
    },

    /// Change a channel's retention policy. Needs `manage` there.
    SetTtl {
        /// Which channel.
        id: String,

        /// The TTL applied when a publisher asks for none, e.g. `7d`.
        #[arg(long)]
        default: Option<String>,

        /// The forced ceiling, e.g. `48h`. `0` lifts it — §7.2bis's own spelling
        /// for "unbounded", so there is one way to say it rather than two.
        #[arg(long)]
        max: Option<String>,
    },
}

/// `notify device …`
#[derive(Debug, clap::Subcommand)]
pub(crate) enum DeviceCommand {
    /// List your devices, revoked ones included.
    List,

    /// Enrol this machine, and remember it in the local store.
    Enrol {
        /// How it should appear in your device list. Defaults to the hostname.
        #[arg(long)]
        label: Option<String>,
    },
}

pub(crate) async fn channel(
    session: &Session,
    command: ChannelCommand,
    output: OutputFormat,
) -> Result<String, CliError> {
    match command {
        ChannelCommand::List => {
            let channels = session.channels().await?;

            match output {
                OutputFormat::Json => render_json(&channels),
                OutputFormat::Table => {
                    if channels.is_empty() {
                        return Ok("no channels — access comes from a group in your identity \
                                   provider, or from an invitation"
                            .to_owned());
                    }
                    Ok(channels
                        .iter()
                        .map(|channel| {
                            format!(
                                "{:<24} {:<10} {} members",
                                channel.id.as_str(),
                                format!("{:?}", channel.origin).to_lowercase(),
                                channel.members.len()
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n"))
                }
            }
        }

        ChannelCommand::Create { id, display_name } => {
            let channel = session.create_channel(&id, display_name.as_deref()).await?;

            match output {
                OutputFormat::Json => render_json(&channel),
                OutputFormat::Table => Ok(format!("created {}", channel.id.as_str())),
            }
        }

        ChannelCommand::Join { id } => {
            // Joining is adding yourself, and *the server* decides whether you may:
            // sending your own principal is the only thing the client knows how to
            // do here (§5.1 — authorisation is derived per request, never locally).
            let me = whoami(session).await?;
            session
                .add_member(&id, me, vec![notify_client_core::types::Scope::Subscribe])
                .await?;

            match output {
                OutputFormat::Json => Ok(format!(r#"{{"joined":"{id}"}}"#)),
                OutputFormat::Table => Ok(format!("joined {id}")),
            }
        }

        ChannelCommand::SetTtl { id, default, max } => {
            if default.is_none() && max.is_none() {
                return Err(CliError::Usage(
                    "nothing to set — pass --default, --max, or both".to_owned(),
                ));
            }

            // Only what was asked for goes on the wire: a `PATCH` that restated the
            // untouched knob is how two admins on two screens overwrite each other.
            let channel = session
                .update_channel_policy(
                    &id,
                    ChannelPolicyBody {
                        default_ttl: default.map(Ttl::from),
                        max_ttl: max.map(Ttl::from),
                        ..ChannelPolicyBody::default()
                    },
                )
                .await?;

            match output {
                OutputFormat::Json => render_json(&channel),
                OutputFormat::Table => Ok(format!(
                    "{}: default {}, ceiling {}",
                    channel.id.as_str(),
                    *channel.policy.ttl.default,
                    channel
                        .policy
                        .ttl
                        .max
                        .as_deref()
                        .map_or_else(|| "none".to_owned(), ToOwned::to_owned),
                )),
            }
        }
    }
}

pub(crate) async fn device(
    session: &Session,
    command: DeviceCommand,
    profile: &str,
    output: OutputFormat,
) -> Result<String, CliError> {
    match command {
        DeviceCommand::List => {
            let devices = session.devices().await?;

            match output {
                OutputFormat::Json => render_json(&devices),
                OutputFormat::Table => Ok(devices
                    .iter()
                    .map(|device| {
                        let state = device
                            .revoked_at
                            .map_or_else(|| "active".to_owned(), |at| format!("revoked {at}"));
                        format!("{:<28} {:<20} {state}", device.id.to_string(), device.label)
                    })
                    .collect::<Vec<_>>()
                    .join("\n")),
            }
        }

        DeviceCommand::Enrol { label } => {
            let label = label.unwrap_or_else(hostname);
            let device = session.enrol_device(&label).await?;

            // Recorded in the shared store, not in this process: the enrolment is
            // per *profile*, so the TUI and the desktop app inherit this device
            // rather than each enrolling their own (§9.3bis). Three devices for one
            // machine would also make "seen by 3/5" count it three times.
            Store::open(profile)?.set_device(&device.id.to_string())?;

            match output {
                OutputFormat::Json => render_json(&device),
                OutputFormat::Table => Ok(format!("enrolled {} as {}", device.id, device.label)),
            }
        }
    }
}

/// `notify ls` — the inbox, most recent first.
pub(crate) async fn list_inbox(
    session: &Session,
    channel: Option<&str>,
    limit: u16,
    output: OutputFormat,
) -> Result<String, CliError> {
    let notifications = session.inbox(channel, limit).await?;

    match output {
        OutputFormat::Json => render_json(&notifications),
        OutputFormat::Table => Ok(notifications
            .iter()
            .map(|entry| {
                let head = &entry.head;
                format!(
                    "{:<10} {:<8} {:<16} {}",
                    format!("{:?}", head.status).to_lowercase(),
                    format!("{:?}", head.state).to_lowercase(),
                    entry.channel.as_str(),
                    head.content.title
                )
            })
            .collect::<Vec<_>>()
            .join("\n")),
    }
}

/// Who the caller is, as a principal the API accepts.
///
/// There is no `GET /users/me` returning a principal, and adding one for this
/// would be a route existing to serve a convenience. The device list is already
/// scoped to the caller, so its owner *is* the answer — and it costs one request
/// the join was going to make anyway.
async fn whoami(session: &Session) -> Result<notify_client_core::types::Principal, CliError> {
    let devices = session.devices().await?;

    devices
        .first()
        .map(|device| device.owner.clone())
        .ok_or_else(|| {
            CliError::Usage(
                "this profile has no device yet, so joining cannot name you — run \
                 `notify device enrol` first"
                    .to_owned(),
            )
        })
}

fn render_json<T: serde::Serialize>(value: &T) -> Result<String, CliError> {
    serde_json::to_string_pretty(value)
        .map_err(|error| CliError::Api(format!("could not render JSON: {error}")))
}

/// This machine's name, for a device label nobody had to type.
fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "this machine".to_owned())
}

/// `notify sync` — catch the local store up (design §7.2).
pub(crate) async fn sync(
    session: &Session,
    profile: &str,
    channel: Option<&str>,
    output: OutputFormat,
) -> Result<String, CliError> {
    let store = Store::open(profile)?;
    let device = store.device()?;

    let outcome = session
        .sync_into(&store, channel, device.as_deref())
        .await?;

    match output {
        OutputFormat::Json => render_json(&serde_json::json!({
            "stored": outcome.stored,
            "removed": outcome.removed,
            "compacted": outcome.compacted,
            "expired": outcome.expired.iter().map(|(channel, count)| {
                serde_json::json!({ "channel": channel, "count": count })
            }).collect::<Vec<_>>(),
        })),
        OutputFormat::Table => {
            let mut lines = vec![if outcome.is_quiet() {
                "already up to date".to_owned()
            } else {
                format!("{} stored, {} removed", outcome.stored, outcome.removed)
            }];

            if outcome.compacted > 0 {
                // §7.2: forty revisions of one deploy arrive as one entry, and the
                // number is what lets a client say "38 updates" rather than
                // pretending nothing happened.
                lines.push(format!(
                    "{} revisions folded away by compaction",
                    outcome.compacted
                ));
            }
            for (channel, count) in &outcome.expired {
                // §7.2bis: a notification that expired undelivered is deliberately
                // not in the backlog, so it is reported as a count instead of
                // vanishing without trace.
                lines.push(format!(
                    "{count} notifications expired in {channel} while you were away"
                ));
            }

            Ok(lines.join("\n"))
        }
    }
}
