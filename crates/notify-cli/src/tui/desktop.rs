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
///
/// **Unverified here.** There is no desktop session in this workspace, so this
/// function has never run against a real daemon (§14.4, D19).
pub(crate) fn relay(title: &str, body: &str) {
    let _ = notify_rust::Notification::new()
        .summary(title)
        .body(body)
        .appname("weebo-notify")
        .show();
}
