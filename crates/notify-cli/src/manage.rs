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

use notify_client_core::types::{ChannelPolicyBody, Day, DndDraft, QuietHours, Ttl, WallClock};
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
    ///
    /// `enroll` is accepted too. §9.1 writes the command with two `l`s, this crate
    /// spells it with one, and the difference is invisible until somebody copies
    /// the line out of the design document and gets "unrecognized subcommand".
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

/// `notify dnd …` — the do-not-disturb of design §7.4.
///
/// §9.1 sketches this as `on|off|until 9h|for 2h|schedule|status`. `until` and `for`
/// are flags of `on` here rather than verbs of their own, because they are two
/// spellings of one action: as subcommands, clap would have to offer a bare
/// `notify dnd until` with nothing to say what it applies to.
#[derive(Debug, clap::Subcommand)]
pub(crate) enum DndCommand {
    /// Show what is set, and whether it is quiet right now.
    Status,

    /// Start a quiet period. Say how long with `--for`, or when it ends with
    /// `--until`.
    On {
        /// How long, e.g. `2h` or `45m`.
        #[arg(long = "for", value_name = "DURATION", conflicts_with = "until")]
        duration: Option<String>,

        /// When it lifts, as an RFC 3339 instant, e.g. `2026-08-05T09:00:00Z`.
        #[arg(long, value_name = "INSTANT")]
        until: Option<String>,

        /// Until the next 08:00 on your own clock — the same choice the tray menu
        /// offers (§10bis.3ter), and the same calculation behind it.
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

        /// The IANA zone the two are read in, e.g. `Europe/Paris`. Yours, never the
        /// machine's — §7.4 keeps somebody's nights theirs when they travel.
        #[arg(long, required_unless_present = "clear")]
        timezone: Option<String>,

        /// Remove every window instead of adding one.
        #[arg(long, conflicts_with_all = ["from", "to", "timezone"])]
        clear: bool,
    },
}

/// Run one `notify dnd …`.
///
/// Every branch reads the current preference before writing: the endpoint replaces
/// it whole (§7.4 carries it whole for the same reason), so a client that wants to
/// change one thing has to send back the rest. Doing that read here rather than in
/// each caller is what stops `dnd on` from quietly wiping somebody's schedule.
pub(crate) async fn dnd(
    session: &Session,
    command: DndCommand,
    output: OutputFormat,
) -> Result<String, CliError> {
    let current = session.dnd().await?;

    // Read before write, because the endpoint replaces the preference whole (§7.4).
    // Carrying the rest across is what stops `dnd on` from wiping a schedule.
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
                    // The preference's zone, not this machine's (§7.4): somebody
                    // travelling keeps their own mornings. A preference with no
                    // schedule has no zone to borrow, and UTC is the honest answer
                    // rather than a guess from the laptop.
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
                    // Refused rather than guessed at: §7.4 makes the quick toggle a
                    // deadline, and an unbounded one is what somebody sets at 3am
                    // and rediscovers a quarter later.
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
                // The three are `required_unless_present = "clear"`, so clap has
                // already refused the combination where any is missing.
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

    // The server validates the window — the timezone against the real IANA
    // database, the ends against each other — and says which field is wrong. Doing
    // it a second time here would be a second set of rules to keep in step.
    let stored = session.set_dnd(&wanted).await?;

    // Rendered through the draft so there is one renderer rather than two that
    // drift: the only field the entity adds is the subject, and it is the caller.
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
///
/// The client's clock and not the server's, which is a real difference and a small
/// one: the two disagree by seconds, and the alternative is an endpoint that takes a
/// duration purely so the arithmetic happens on the other side. The clock read
/// itself is `notify_client_core::now_utc` — the desktop shell needs exactly the
/// same value, so it lives in the one crate both depend on rather than as a private
/// copy here.
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
