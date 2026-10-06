//! A stub `org.freedesktop.Notifications` daemon, so `--notify-send` can be asserted on in a
//! workspace without a desktop (`task desktop:bus`). A test instrument, not a desktop.

use std::io::Write as _;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use dbus::arg::PropMap;
use dbus::blocking::Connection;
use dbus::{Message, MessageType};
use serde::Serialize;

/// The bus name a notification daemon owns, and the object it answers on.
const BUS_NAME: &str = "org.freedesktop.Notifications";
const OBJECT_PATH: &str = "/org/freedesktop/Notifications";

/// What one `Notify` call carried. The whole `susssasa{sv}i` tuple is read so a malformed
/// message fails rather than being half-decoded.
#[derive(Debug, Serialize)]
struct Delivered {
    app_name: String,
    replaces_id: u32,
    icon: String,
    summary: String,
    body: String,
    actions: Vec<String>,
    expire_timeout: i32,
}

/// Run until `expect` notifications have arrived (or forever), printing each as a JSON line.
pub(crate) fn run(expect: Option<u32>, timeout: Option<Duration>) -> anyhow::Result<()> {
    let connection = Connection::new_session().context(
        "connecting to the session bus — is DBUS_SESSION_BUS_ADDRESS set? \
         `task desktop:notifyd` starts one in the browser sidecar",
    )?;

    // `DO_NOT_QUEUE`: a second instance must fail rather than sit queued looking healthy.
    let reply = connection
        .request_name(BUS_NAME, false, true, false)
        .with_context(|| format!("requesting the bus name {BUS_NAME}"))?;
    if reply != dbus::blocking::stdintf::org_freedesktop_dbus::RequestNameReply::PrimaryOwner {
        anyhow::bail!(
            "{BUS_NAME} is already owned on this bus ({reply:?}) — \
             a real notification daemon is running, and this stub will not replace it"
        );
    }

    eprintln!("owning {BUS_NAME}; waiting for notifications");

    let started = Instant::now();
    let mut seen = 0u32;

    loop {
        if let Some(expect) = expect
            && seen >= expect
        {
            return Ok(());
        }
        if let Some(timeout) = timeout
            && started.elapsed() >= timeout
        {
            anyhow::bail!("timed out after {seen} notification(s); expected {expect:?}");
        }

        let Some(message) = connection
            .channel()
            .blocking_pop_message(Duration::from_millis(200))
            .context("reading from the session bus")?
        else {
            continue;
        };

        if message.msg_type() != MessageType::MethodCall {
            continue;
        }

        if let Some(reply) = answer(&message, &mut seen)? {
            let _ = connection.channel().send(reply);
        }
    }
}

/// Answer one method call, or `None` for a call this stub does not implement.
fn answer(message: &Message, seen: &mut u32) -> anyhow::Result<Option<Message>> {
    let member = message.member().map(|m| m.to_string()).unwrap_or_default();
    let interface = message
        .interface()
        .map(|i| i.to_string())
        .unwrap_or_default();

    if message.path().map(|p| p.to_string()).as_deref() != Some(OBJECT_PATH) {
        return Ok(None);
    }

    // Answered because an unanswered Ping looks like a hung daemon.
    if interface == "org.freedesktop.DBus.Peer" && member == "Ping" {
        return Ok(Some(message.method_return()));
    }

    if interface != BUS_NAME {
        return Ok(None);
    }

    match member.as_str() {
        "Notify" => {
            // Eight arguments, past dbus's `readN` helpers; read in order so a short message fails.
            let mut iter = message.iter_init();
            let decode =
                |what: &'static str| move |error| anyhow::anyhow!("Notify.{what}: {error}");

            let app_name: String = iter.read().map_err(decode("app_name"))?;
            let replaces_id: u32 = iter.read().map_err(decode("replaces_id"))?;
            let icon: String = iter.read().map_err(decode("app_icon"))?;
            let summary: String = iter.read().map_err(decode("summary"))?;
            let body: String = iter.read().map_err(decode("body"))?;
            let actions: Vec<String> = iter.read().map_err(decode("actions"))?;
            let _hints: PropMap = iter.read().map_err(decode("hints"))?;
            let expire_timeout: i32 = iter.read().map_err(decode("expire_timeout"))?;

            let delivered = Delivered {
                app_name,
                replaces_id,
                icon,
                summary,
                body,
                actions,
                expire_timeout,
            };

            *seen += 1;
            // Flushed per line for callers reading it as a stream.
            println!(
                "{}",
                serde_json::to_string(&delivered).context("rendering the notification as JSON")?
            );
            let _ = std::io::stdout().flush();

            Ok(Some(message.method_return().append1(*seen)))
        }

        // Advertise nothing beyond the base protocol, which is all this stub honours.
        "GetCapabilities" => Ok(Some(message.method_return().append1(Vec::<String>::new()))),

        "GetServerInformation" => Ok(Some(
            message
                .method_return()
                .append3("weebo-notify-xtask", "weebo", env!("CARGO_PKG_VERSION"))
                // The specification version this answers for, not ours.
                .append1("1.2"),
        )),

        "CloseNotification" => Ok(Some(message.method_return())),

        _ => Ok(None),
    }
}
