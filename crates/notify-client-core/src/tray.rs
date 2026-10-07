//! What the tray icon shows (§10bis.3ter), kept pure so the tray, dock badge and web sidebar
//! share one state and it is testable without a desktop. No animation, no number in the icon.

/// Whether the shell is currently talking to the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    /// The stream is up.
    Connected,
    /// It is not, and the menu offers "reconnexion…" rather than a popup loop.
    Disconnected,
}

/// Which icon variant the tray shows (§10bis.3ter); named by meaning (macOS uses template images).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayIcon {
    /// At rest, everything read.
    Neutral,
    /// Something unread: the accent dot.
    Unread,
    /// Do-not-disturb or a silence is in force (§7.4).
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

/// Decide what the tray shows. Precedence: disconnected (the unread count is stale) beats
/// do-not-disturb (explains the quiet) beats unread.
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

/// The disabled first line of the tray menu, spelling out every state the icon shows.
fn status_line(unread: u64, quiet: bool, link: Link) -> String {
    // French agrees the count ("non-lu" vs "non-lus").
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
        assert_eq!(tray(3, true, Link::Connected).icon, TrayIcon::DoNotDisturb);
    }

    #[test]
    fn being_disconnected_outranks_everything() {
        // Disconnected can't vouch for its own unread count.
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
        assert_eq!(
            tray(2, true, Link::Disconnected).status,
            "2 non-lus · déconnecté · ne pas déranger"
        );
    }
}
