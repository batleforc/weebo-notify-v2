//! Relaying arrivals to the desktop (design §9.2, `--notify-send`).
//!
//! # Who decides whether it makes a noise
//!
//! Not this file. §7.4 says a connected client suppresses its own popups during a
//! quiet period — the stream keeps flowing, because an open app is a deliberate
//! choice to look — and this client used to answer that question itself, with a
//! copy of the rule the server already applies to push. Three copies existed at the
//! worst point: the domain, this, and the browser (§14.4, D22).
//!
//! The server now says so on the event: `muted_by_dnd`, computed per reader from
//! the preference it already holds. What is left here is the dispatch.
//!
//! # What §9.2 promised and this does not do
//!
//! §9.2 said the relay happens "quand le terminal n'a pas le focus". **There is no
//! portable way to know that.** X11, Wayland, macOS and Windows each answer
//! differently or not at all, and a terminal multiplexer breaks whichever answer
//! one picks. The flag is the switch instead, and it is the person's to turn off —
//! the alternative is a heuristic that is wrong on somebody else's setup and cannot
//! be debugged from here (§14.4, D19).

/// Hand one arrival to the desktop.
///
/// Errors are swallowed on purpose: a machine with no notification daemon — a
/// headless server, a container, a terminal over ssh — is the normal case for this
/// CLI, and a TUI that reported a failed popup on every arrival would be worse than
/// one that quietly does not pop.
pub(crate) fn relay(title: &str, body: &str) {
    let _ = show(title, body);
}

/// The same call, with its outcome kept.
///
/// Split out of [`relay`] for one reason: a function that swallows its own errors
/// can never be verified by anything. D19 read "écrite, jamais vue" for that as much
/// as for the missing desktop — there was no daemon here *and* no way to tell a
/// delivery from a no-op. The swallowing stays, one level up, where it belongs.
fn show(
    title: &str,
    body: &str,
) -> Result<notify_rust::NotificationHandle, notify_rust::error::Error> {
    notify_rust::Notification::new()
        .summary(title)
        .body(body)
        .appname("weebo-notify")
        .show()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deliver to a real notification daemon over a real session bus (D19).
    ///
    /// Skipped unless `DBUS_SESSION_BUS_ADDRESS` names a bus, exactly as the
    /// Postgres conformance suite skips without `WNK_TEST_DATABASE_URL`: the
    /// developer machines that have a desktop session run it for free, and this
    /// workspace gets one from `task desktop:check`.
    ///
    /// What it proves is narrow and worth stating: that what §9.2 sends is a
    /// message a daemon accepts. `xtask notifyd` decodes all eight arguments of the
    /// specification's `susssasa{sv}i` strictly, so a malformed one fails there
    /// rather than being drawn as an empty popup.
    #[test]
    fn an_arrival_reaches_a_real_notification_daemon() {
        let Ok(address) = std::env::var("DBUS_SESSION_BUS_ADDRESS") else {
            eprintln!("skipped: no DBUS_SESSION_BUS_ADDRESS — `task desktop:notifyd` starts one");
            return;
        };

        let delivered = show("etcd degraded", "cluster-ovh-1 · haute");

        assert!(
            delivered.is_ok(),
            "the desktop relay of §9.2 did not reach the daemon on {address}: {:?}",
            delivered.err()
        );
    }
}
