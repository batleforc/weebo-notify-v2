//! A notification daemon, so that `--notify-send` can finally be *seen* (D19).
//!
//! # Why this exists at all
//!
//! `notify-cli --notify-send` (design §9.2) hands each arrival to the desktop over
//! D-Bus, and `crates/notify-cli/src/tui/desktop.rs` has carried the same admission
//! since M4quaterdecies: "there is no desktop session in this workspace, so this
//! function has never run against a real daemon". D21 and D26 were closed the same
//! way — by looking — and this is the third of the three, the one M4quatervicies
//! said a browser could not help with, because `--notify-send` targets a daemon and
//! not a page.
//!
//! # Why a daemon of our own rather than dunst
//!
//! Because neither exists here and neither can be installed: this workspace has no
//! root (§12.4), `mise` carries no `dunst`, `mako` or `dbus-daemon`, and the pod's
//! security context applies to every container, so a sidecar would not change that.
//! What the workspace *does* have is a session bus — `dbus-daemon` ships inside the
//! `browser` sidecar, and both containers mount the sources, so a socket on that
//! shared path is reachable from here (`task desktop:bus`).
//!
//! The remaining gap was a process owning `org.freedesktop.Notifications`, which is
//! four methods of a published specification. That is the same trade `dev/browser/
//! cdp.mjs` made against Playwright: the protocol is small, the dependency is not,
//! and a stub we own can *assert* what arrived — which is the entire point here, and
//! precisely what a real daemon cannot do, since it would only draw a popup nobody
//! in this environment can look at.
//!
//! **It is a test instrument, not a desktop.** It proves the wire contract — that a
//! real bus accepts what `notify-rust` sends, and that the summary, body and appname
//! arrive intact. It does not prove that GNOME draws it nicely, and nothing runnable
//! from this pod could.

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

/// What one `Notify` call carried.
///
/// Only the fields §9.2 actually sends are named. The specification's signature is
/// `susssasa{sv}i`, and the rest is read and dropped rather than ignored: reading
/// the whole tuple is what proves the message was well formed, and a decode that
/// stopped at the third field would accept a malformed one.
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

/// Run until `expect` notifications have arrived, or for ever when it is `None`.
///
/// Each one is printed as a single JSON line, so a script or a test can assert on
/// what the daemon saw rather than on the fact that a call did not error — the
/// distinction D19 is about, since `relay()` swallows its errors by design.
pub(crate) fn run(expect: Option<u32>, timeout: Option<Duration>) -> anyhow::Result<()> {
    let connection = Connection::new_session().context(
        "connecting to the session bus — is DBUS_SESSION_BUS_ADDRESS set? \
         `task desktop:notifyd` starts one in the browser sidecar",
    )?;

    // Without `DO_NOT_QUEUE` a second instance would sit in the queue looking
    // healthy while the first one answered everything, which is the confusing
    // version of this failure.
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
            // A failed send is not worth stopping for: the caller has already been
            // told what arrived, which is what this instrument is for.
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

    // `org.freedesktop.DBus.Peer` is answered because a client is entitled to ping
    // before it trusts us, and an unanswered Ping looks exactly like a hung daemon.
    if interface == "org.freedesktop.DBus.Peer" && member == "Ping" {
        return Ok(Some(message.method_return()));
    }

    if interface != BUS_NAME {
        return Ok(None);
    }

    match member.as_str() {
        "Notify" => {
            // The signature is `susssasa{sv}i` — eight arguments, where this crate's
            // `readN` helpers stop at five. Driving the iterator is not a workaround
            // for that: every field is read in order, so a message that is short or
            // mistyped fails here rather than being silently half-decoded.
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
            // One JSON object per line, flushed: a caller reading this as a stream
            // must not wait on a buffer that fills at some other pace.
            println!(
                "{}",
                serde_json::to_string(&delivered).context("rendering the notification as JSON")?
            );
            let _ = std::io::stdout().flush();

            // The id is what the specification returns, and it has to be non-zero
            // and unique enough that `replaces_id` means something.
            Ok(Some(message.method_return().append1(*seen)))
        }

        // Advertised as supporting nothing beyond the base protocol. Saying "body"
        // or "actions" here would be a claim this stub does not honour, and a client
        // that believed it would format for a renderer that does not exist.
        "GetCapabilities" => Ok(Some(message.method_return().append1(Vec::<String>::new()))),

        "GetServerInformation" => Ok(Some(
            message
                .method_return()
                .append3("weebo-notify-xtask", "weebo", env!("CARGO_PKG_VERSION"))
                // The specification version this answers for, not ours.
                .append1("1.2"),
        )),

        // Nothing is drawn, so nothing can be closed. Answered rather than refused,
        // because a client tidying up after itself should not see an error.
        "CloseNotification" => Ok(Some(message.method_return())),

        _ => Ok(None),
    }
}
