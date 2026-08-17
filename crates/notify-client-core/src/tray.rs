//! What the tray icon shows, and why (design §10bis.3ter).
//!
//! # Why this is here and not in the shell
//!
//! §10bis.3ter ends on the rule this module exists to keep: the unread counter of
//! the tray, of the dock/taskbar badge (§10bis.3bis) and of the web sidebar (§10.4)
//! come from "le **même état** de `notify-client-core` — trois rendus, une seule
//! vérité". A state machine written inside the Tauri shell would be a fourth truth,
//! and the one nobody could test without a desktop session.
//!
//! It is deliberately *pure*: counts and flags in, an icon and a line of text out.
//! Everything that needs a window, a GTK stack or an OS tray lives in the shell and
//! binds to this — which is what lets the interesting half be tested on a machine
//! that cannot build the shell at all (§12.4, D13).
//!
//! # What it does not do
//!
//! No animation, no blinking: §10bis.3ter is explicit that *the icon is the
//! information*. And no number burnt into the icon — illegible at 16 px, and the
//! count belongs in the menu line below.

/// Whether the shell is currently talking to the server.
///
/// Its own type rather than a `bool` because the tray renders it and the status
/// line spells it, and `connected: false` at two call sites is how one of them ends
/// up meaning "unknown".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    /// The stream is up.
    Connected,
    /// It is not, and the menu offers "reconnexion…" rather than a popup loop.
    Disconnected,
}

/// Which icon variant the tray shows (design §10bis.3ter).
///
/// On macOS these are **template** images — monochrome, so the OS follows light and
/// dark and the Retina menu bar renders them correctly. That is a property of the
/// asset, not of this enum, but it is why the variants are named by *meaning* and
/// never by colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayIcon {
    /// At rest, everything read.
    Neutral,
    /// Something unread: the accent dot.
    Unread,
    /// Do-not-disturb or a silence is in force — the moon/barred variant, so that
    /// "why is it not ringing" is answerable at a glance (§7.4).
    DoNotDisturb,
    /// Not talking to the server: the greyed variant.
    Disconnected,
}

/// Everything the tray needs to draw itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tray {
    /// The icon variant.
    pub icon: TrayIcon,
    /// How many notifications are unread, for the menu line.
    pub unread: u64,
    /// The disabled first line of the menu — `3 non-lus · connecté` (§10bis.3ter).
    pub status: String,
}

/// Decide what the tray shows.
///
/// # The precedence, which the design does not state and which has to be chosen
///
/// Disconnected beats do-not-disturb beats unread. The reasoning is what the icon
/// is *for*: each non-neutral variant answers "why is it quiet?", and the answers
/// are not equally urgent. Being disconnected is a **fault** — the person did not
/// ask for it, and worse, the unread count they can see is stale by definition, so
/// showing the accent dot would be showing a number this client cannot vouch for.
/// Do-not-disturb is a **choice**; it explains the quiet completely, and it outranks
/// the unread dot for the same reason — the dot would invite "why did nothing pop?"
/// when the answer is already on screen.
#[must_use]
pub fn tray(unread: u64, quiet: bool, link: Link) -> Tray {
    let icon = match (link, quiet, unread) {
        (Link::Disconnected, _, _) => TrayIcon::Disconnected,
        (Link::Connected, true, _) => TrayIcon::DoNotDisturb,
        (Link::Connected, false, 0) => TrayIcon::Neutral,
        (Link::Connected, false, _) => TrayIcon::Unread,
    };

    Tray {
        icon,
        unread,
        status: status_line(unread, quiet, link),
    }
}

/// The informative, disabled first line of the tray menu.
///
/// Every state the icon can be in is also spelled out here, on purpose: an icon
/// carries about two bits, and somebody hovering the tray because they are puzzled
/// is exactly the person the shorthand fails.
fn status_line(unread: u64, quiet: bool, link: Link) -> String {
    // French agrees the count. The web sidebar learned this the hard way in
    // M4quinvicies, where the badge read "1 non lues" until somebody opened the
    // page; §10bis.3ter spells the tray's own form with a hyphen, so it is
    // "non-lu" here and "non lue" there — the same rule, two nouns.
    let count = if unread == 1 {
        "1 non-lu".to_owned()
    } else {
        format!("{unread} non-lus")
    };

    let mut spelled = match link {
        Link::Connected => format!("{count} · connecté"),
        Link::Disconnected => format!("{count} · déconnecté"),
    };
    if quiet {
        spelled.push_str(" · ne pas déranger");
    }
    spelled
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_rest_and_all_read_is_the_neutral_icon() {
        assert_eq!(tray(0, false, Link::Connected).icon, TrayIcon::Neutral);
    }

    #[test]
    fn something_unread_earns_the_accent_dot() {
        assert_eq!(tray(3, false, Link::Connected).icon, TrayIcon::Unread);
    }

    #[test]
    fn a_quiet_period_outranks_the_unread_dot() {
        // Both are true at once all the time — that is the normal state of a
        // do-not-disturb evening. The dot would invite "why did nothing pop?" when
        // the answer is the other variant.
        assert_eq!(tray(3, true, Link::Connected).icon, TrayIcon::DoNotDisturb);
    }

    #[test]
    fn being_disconnected_outranks_everything() {
        // The strongest of the three, and the least obvious. A client that is not
        // connected cannot vouch for its own unread count, so showing the accent
        // dot would be showing a number that is stale by construction.
        assert_eq!(
            tray(3, true, Link::Disconnected).icon,
            TrayIcon::Disconnected
        );
        assert_eq!(
            tray(0, false, Link::Disconnected).icon,
            TrayIcon::Disconnected
        );
    }

    #[test]
    fn the_status_line_agrees_with_the_count() {
        assert_eq!(
            tray(1, false, Link::Connected).status,
            "1 non-lu · connecté"
        );
        assert_eq!(
            tray(3, false, Link::Connected).status,
            "3 non-lus · connecté"
        );
        assert_eq!(
            tray(0, false, Link::Connected).status,
            "0 non-lus · connecté"
        );
    }

    #[test]
    fn the_status_line_says_every_state_the_icon_only_hints_at() {
        // An icon carries about two bits. Somebody hovering the tray because they
        // are puzzled is precisely the person the shorthand fails.
        assert_eq!(
            tray(2, true, Link::Disconnected).status,
            "2 non-lus · déconnecté · ne pas déranger"
        );
    }
}
