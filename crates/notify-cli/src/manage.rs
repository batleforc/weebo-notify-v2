//! Channels, devices, the inbox and DND (§9.1): renderings of `notify-client-core` calls that
//! honour `--output json|table` and return their output, so the TUI's `:` mode can show it (§9.3).

use notify_client_core::types::{
    ChannelPolicyBody, CloseAfter, Day, DndDraft, QuietHours, Ttl, WallClock,
};
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

        /// The forced ceiling, e.g. `48h`; `0` lifts it (§7.2bis).
        #[arg(long)]
        max: Option<String>,
    },

    /// Change how long a lifecycle notification may go without activity before the server
    /// closes it. Needs `manage` there.
    SetCloseAfter {
        /// Which channel.
        id: String,

        /// How long without a revision or an interaction, e.g. `7d`; `0` turns it off.
        after: String,
    },
}

/// `notify device …`
#[derive(Debug, clap::Subcommand)]
pub(crate) enum DeviceCommand {
    /// List your devices, revoked ones included.
    List,

    /// Enrol this machine, and remember it in the local store (`enroll` works too).
    #[command(alias = "enroll")]
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
            // The server decides whether you may join (§5.1); the client only names itself.
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

            // Only the changed knob goes on the wire, so concurrent edits don't overwrite each other.
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

        ChannelCommand::SetCloseAfter { id, after } => {
            set_close_after(session, &id, after, output).await
        }
    }
}

/// `notify channel set-close-after <id> <after>`.
async fn set_close_after(
    session: &Session,
    id: &str,
    after: String,
    output: OutputFormat,
) -> Result<String, CliError> {
    let channel = session
        .update_channel_policy(
            id,
            ChannelPolicyBody {
                close_after: Some(CloseAfter::from(after)),
                ..ChannelPolicyBody::default()
            },
        )
        .await?;

    match output {
        OutputFormat::Json => render_json(&channel),
        // The server spells "never" as `0s`; don't print it as "close instantly".
        OutputFormat::Table => {
            let close_after = &*channel.policy.close_after;
            Ok(format!(
                "{}: closes after {}",
                channel.id.as_str(),
                if close_after == "0s" {
                    "never — publishers close their own"
                } else {
                    close_after
                },
            ))
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

            // Per profile, so the TUI and desktop app reuse this device (§9.3bis).
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

/// Who the caller is, as a principal: the owner of its (caller-scoped) device list.
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
                // §7.2: revisions folded by compaction.
                lines.push(format!(
                    "{} revisions folded away by compaction",
                    outcome.compacted
                ));
            }
            for (channel, count) in &outcome.expired {
                // §7.2bis: expired undelivered, reported as a count rather than vanishing.
                lines.push(format!(
                    "{count} notifications expired in {channel} while you were away"
                ));
            }

            Ok(lines.join("\n"))
        }
    }
}

/// `notify dnd …` — the do-not-disturb of §7.4 (`--for`/`--until` are flags of `on`).
#[derive(Debug, clap::Subcommand)]
pub(crate) enum DndCommand {
    /// Show what is set, and whether it is quiet right now.
    Status,

    /// Start a quiet period: how long with `--for`, or when it ends with `--until`.
    On {
        /// How long, e.g. `2h` or `45m`.
        #[arg(long = "for", value_name = "DURATION", conflicts_with = "until")]
        duration: Option<String>,

        /// When it lifts, as an RFC 3339 instant, e.g. `2026-08-05T09:00:00Z`.
        #[arg(long, value_name = "INSTANT")]
        until: Option<String>,

        /// Until the next 08:00 on your own clock, as the tray menu offers (§10bis.3ter).
        #[arg(long, conflicts_with_all = ["duration", "until"])]
        until_morning: bool,
    },

    /// End the quiet period now. Leaves the weekly schedule alone.
    Off,

    /// Replace the weekly quiet hours.
    Schedule {
        /// Days it applies to, e.g. `mon,tue,fri`. Omitted means every day.
        #[arg(long, value_delimiter = ',')]
        days: Vec<String>,

        /// When quiet begins, on the local clock, e.g. `22:00`.
        #[arg(long, required_unless_present = "clear")]
        from: Option<String>,

        /// When it ends, e.g. `08:00`. Earlier than `--from` crosses midnight.
        #[arg(long, required_unless_present = "clear")]
        to: Option<String>,

        /// The IANA zone the two are read in, e.g. `Europe/Paris` (yours, not the machine's, §7.4).
        #[arg(long, required_unless_present = "clear")]
        timezone: Option<String>,

        /// Remove every window instead of adding one.
        #[arg(long, conflicts_with_all = ["from", "to", "timezone"])]
        clear: bool,
    },
}

/// Run one `notify dnd …`.
pub(crate) async fn dnd(
    session: &Session,
    command: DndCommand,
    output: OutputFormat,
) -> Result<String, CliError> {
    let current = session.dnd().await?;

    // The endpoint replaces the preference whole (§7.4): read first so `dnd on` keeps the schedule.
    let current = DndDraft {
        quiet_hours: current.quiet_hours,
        until: current.until,
        scope: current.scope,
        exceptions: current.exceptions,
        urgent_bypasses: current.urgent_bypasses,
    };

    let wanted = match command {
        DndCommand::Status => return render_draft(&current, output),

        DndCommand::On {
            duration,
            until,
            until_morning,
        } => {
            let until = match (duration, until, until_morning) {
                (Some(duration), _, _) => now_plus(&duration)?,
                (None, Some(instant), _) => instant.parse().map_err(|_| {
                    CliError::Usage(
                        "`--until` wants an RFC 3339 instant, e.g. `2026-08-05T09:00:00Z`"
                            .to_owned(),
                    )
                })?,
                (None, None, true) => {
                    // The preference's zone, not this machine's (§7.4); UTC when there is none.
                    let timezone = current
                        .quiet_hours
                        .first()
                        .map_or("UTC", |window| window.timezone.as_str());

                    notify_client_core::DndPreset::UntilMorning
                        .until(notify_client_core::now_utc()?, timezone)?
                        .ok_or_else(|| {
                            CliError::Usage("`--until-morning` produced no deadline".to_owned())
                        })?
                }
                (None, None, false) => {
                    // §7.4 makes the toggle a deadline; an unbounded one is refused.
                    return Err(CliError::Usage(
                        "say how long: `--for 2h`, `--until <instant>`, or `--until-morning`"
                            .to_owned(),
                    ));
                }
            };
            DndDraft {
                until: Some(until),
                ..current
            }
        }

        DndCommand::Off => DndDraft {
            until: None,
            ..current
        },

        DndCommand::Schedule {
            days,
            from,
            to,
            timezone,
            clear,
        } => {
            if clear {
                DndDraft {
                    quiet_hours: Vec::new(),
                    ..current
                }
            } else {
                // clap already requires all three unless `--clear`.
                let (Some(from), Some(to), Some(timezone)) = (from, to, timezone) else {
                    return Err(CliError::Usage(
                        "a window needs `--from`, `--to` and `--timezone`".to_owned(),
                    ));
                };

                DndDraft {
                    quiet_hours: vec![QuietHours {
                        days: days
                            .iter()
                            .map(|day| parse_day(day))
                            .collect::<Result<_, _>>()?,
                        start: WallClock::from(from),
                        end: WallClock::from(to),
                        timezone,
                    }],
                    ..current
                }
            }
        }
    };

    // The server validates the window and names the bad field.
    let stored = session.set_dnd(&wanted).await?;

    // One renderer: the entity only adds the subject, which is the caller.
    render_draft(
        &DndDraft {
            quiet_hours: stored.quiet_hours,
            until: stored.until,
            scope: stored.scope,
            exceptions: stored.exceptions,
            urgent_bypasses: stored.urgent_bypasses,
        },
        output,
    )
}

/// Render a preference, in both formats.
fn render_draft(dnd: &DndDraft, output: OutputFormat) -> Result<String, CliError> {
    match output {
        OutputFormat::Json => render_json(dnd),
        OutputFormat::Table => {
            let mut lines = Vec::new();

            match &dnd.until {
                Some(until) => lines.push(format!("quiet until {until}")),
                None => lines.push("no quiet period running".to_owned()),
            }

            for window in &dnd.quiet_hours {
                let days = if window.days.is_empty() {
                    "every day".to_owned()
                } else {
                    window
                        .days
                        .iter()
                        .map(|day| format!("{day:?}").to_lowercase())
                        .collect::<Vec<_>>()
                        .join(",")
                };
                lines.push(format!(
                    "{days}  {}–{}  {}",
                    window.start.as_str(),
                    window.end.as_str(),
                    window.timezone
                ));
            }

            if !dnd.exceptions.is_empty() {
                lines.push(format!(
                    "always rings: {}",
                    dnd.exceptions
                        .iter()
                        .map(|channel| channel.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if dnd.urgent_bypasses == Some(true) {
                lines.push("`urgent` is allowed through".to_owned());
            }

            Ok(lines.join("\n"))
        }
    }
}

/// A day name as §7.4 spells it.
fn parse_day(raw: &str) -> Result<Day, CliError> {
    match raw.trim().to_lowercase().as_str() {
        "mon" => Ok(Day::Mon),
        "tue" => Ok(Day::Tue),
        "wed" => Ok(Day::Wed),
        "thu" => Ok(Day::Thu),
        "fri" => Ok(Day::Fri),
        "sat" => Ok(Day::Sat),
        "sun" => Ok(Day::Sun),
        other => Err(CliError::Usage(format!(
            "`{other}` is not a day — expected mon, tue, wed, thu, fri, sat or sun"
        ))),
    }
}

/// `--for 2h`, resolved against this machine's clock.
fn now_plus(duration: &str) -> Result<chrono::DateTime<chrono::Utc>, CliError> {
    let (value, unit) = duration.split_at(
        duration
            .find(|character: char| !character.is_ascii_digit())
            .ok_or_else(|| {
                CliError::Usage("`--for` wants a unit, e.g. `2h`, `45m` or `30s`".to_owned())
            })?,
    );
    let value: i64 = value
        .parse()
        .map_err(|_| CliError::Usage(format!("`{duration}` is not a duration, e.g. `2h`")))?;

    let seconds = match unit {
        "s" => value,
        "m" => value * 60,
        "h" => value * 3_600,
        "d" => value * 86_400,
        other => {
            return Err(CliError::Usage(format!(
                "`{other}` is not a unit — expected s, m, h or d"
            )));
        }
    };

    notify_client_core::now_utc()?
        .checked_add_signed(chrono::TimeDelta::seconds(seconds))
        .ok_or_else(|| CliError::Usage(format!("`{duration}` is further away than time goes")))
}
