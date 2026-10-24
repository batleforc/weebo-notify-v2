//! Relaying arrivals to the desktop (§9.2, `--notify-send`). The server decides DND muting
//! (`muted_by_dnd`); terminal focus can't be detected portably, so the flag is the switch.

/// Hand one arrival to the desktop. Errors are swallowed: no notification daemon (ssh,
/// containers) is the normal case.
#[expect(
    clippy::let_underscore_must_use,
    let_underscore_drop,
    reason = "no notification daemon (ssh, containers) is the normal case"
)]
pub(crate) fn relay(title: &str, body: &str) {
    let _ = show(title, body);
}

/// The same call with its outcome kept, so delivery can be tested.
fn show(
    title: &str,
    body: &str,
) -> Result<notify_rust::NotificationHandle, notify_rust::error::Error> {
    notify_rust::Notification::new()
        .summary(title)
        .body(&plain(body))
        .appname("weebo-notify")
        .show()
}

/// A publisher's text as text: daemons read the body as markup, where `<a>` or `<img>`
/// would turn a notification into a link or a remote fetch.
fn plain(body: &str) -> String {
    body.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_in_a_body_reaches_the_daemon_as_text() {
        assert_eq!(
            plain(r#"<img src="https://x/y.png"> & <b>"#),
            r#"&lt;img src="https://x/y.png"&gt; &amp; &lt;b&gt;"#
        );
    }

    /// Deliver to a real notification daemon over a real session bus; skipped unless
    /// `DBUS_SESSION_BUS_ADDRESS` is set (`task desktop:check` provides one).
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
