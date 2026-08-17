//! `weebo-notify://` links, and where they land (design §10bis.3quater).
//!
//! # Why this is a mirror and not a scheme of its own
//!
//! §10bis.3quater asks for "miroir exact des permaliens web (§10.4)": the web serves
//! `/n/<id>` and `/c/<id>`, so the desktop answers `weebo-notify://n/<id>` and
//! `weebo-notify://c/<id>`. That is the whole design — one address shape, two
//! transports — and it is what lets a link in a sink email (§7.1bis), an OS
//! notification click or a CI log open the right screen in whichever client the
//! person has.
//!
//! # Why parsing it is not "just split on slash"
//!
//! Because this is an **input from outside the app**: the OS hands it over from a
//! click somebody else authored. A parser that shrugged and passed the tail through
//! would let a crafted link steer the shell wherever its router happens to go. So
//! this refuses everything it does not recognise, and returns a value the router can
//! only use one way.
//!
//! Held here rather than in the shell for the reason the tray is: it is pure, and
//! the shell cannot be built on a machine without the GTK stack (§12.4, D13) while
//! this can be tested anywhere.

use url::Url;

/// Where a `weebo-notify://` link points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeepLink {
    /// `weebo-notify://n/<id>` — open the detail panel, like `/n/<id>` on the web.
    Notification(String),
    /// `weebo-notify://c/<id>` — narrow the inbox to a channel, like `/c/<id>`.
    Channel(String),
}

/// The scheme the packages register.
pub const SCHEME: &str = "weebo-notify";

impl DeepLink {
    /// The web permalink path this link mirrors.
    ///
    /// The shell routes with this rather than matching on the variant again: the
    /// Vue router already knows these two paths, and re-deriving them in the shell
    /// is how the two drift.
    #[must_use]
    pub fn path(&self) -> String {
        match self {
            Self::Notification(id) => format!("/n/{id}"),
            Self::Channel(id) => format!("/c/{id}"),
        }
    }
}

/// Read a link the OS handed over, or refuse it.
///
/// `None` for anything that is not exactly one of the two shapes. There is no
/// lenient branch on purpose — see the module note on where these values come from.
#[must_use]
pub fn parse_deep_link(raw: &str) -> Option<DeepLink> {
    let url = Url::parse(raw).ok()?;

    // Schemes are case-insensitive (RFC 3986) and `Url` already lowercases them,
    // so this comparison is the whole check.
    if url.scheme() != SCHEME {
        return None;
    }

    // `weebo-notify://n/<id>` puts `n` in the host position, not the path — which is
    // surprising enough that reading it wrong is the likely bug here rather than the
    // unlikely one.
    let kind = url.host_str()?;

    let mut segments = url.path_segments()?;
    let id = segments.next()?;
    // Exactly one segment. A trailing slash leaves an empty one, which is the
    // ordinary way a link gets copied, so that single case is allowed through.
    if segments.any(|extra| !extra.is_empty()) {
        return None;
    }

    // An **allowlist**, and the first draft of this was a blocklist of separators
    // that its own test broke: `path_segments` hands back the segment still
    // percent-encoded, so `%2F..%2Fadmin` contains no `/` at all here and would
    // decode to one in whatever builds the next address. Listing what ids are made
    // of — a prefixed ULID, a channel slug — cannot be wrong in that direction.
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return None;
    }

    match kind {
        "n" => Some(DeepLink::Notification(id.to_owned())),
        "c" => Some(DeepLink::Channel(id.to_owned())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_shapes_mirror_the_web_permalinks() {
        assert_eq!(
            parse_deep_link("weebo-notify://n/ntf_01KZDXXHXK4KFHJ8NDR63CA8RK"),
            Some(DeepLink::Notification(
                "ntf_01KZDXXHXK4KFHJ8NDR63CA8RK".to_owned()
            ))
        );
        assert_eq!(
            parse_deep_link("weebo-notify://c/ci-infra"),
            Some(DeepLink::Channel("ci-infra".to_owned()))
        );
    }

    #[test]
    fn a_link_knows_the_web_path_it_mirrors() {
        // The shell routes with this instead of matching the variant a second time,
        // which is how the desktop and the web would drift apart.
        assert_eq!(
            DeepLink::Notification("ntf_1".to_owned()).path(),
            "/n/ntf_1"
        );
        assert_eq!(
            DeepLink::Channel("ci-infra".to_owned()).path(),
            "/c/ci-infra"
        );
    }

    #[test]
    fn a_trailing_slash_is_how_links_get_copied() {
        assert_eq!(
            parse_deep_link("weebo-notify://c/ci-infra/"),
            Some(DeepLink::Channel("ci-infra".to_owned()))
        );
    }

    #[test]
    fn anything_that_is_not_one_of_the_two_shapes_is_refused() {
        // This is an input from outside the app: the OS hands it over from a click
        // somebody else authored, so a lenient branch is a way to steer the shell.
        assert_eq!(
            parse_deep_link("https://weebo.poc/n/ntf_1"),
            None,
            "wrong scheme"
        );
        assert_eq!(
            parse_deep_link("weebo-notify://x/ntf_1"),
            None,
            "unknown kind"
        );
        assert_eq!(parse_deep_link("weebo-notify://n/"), None, "no id");
        assert_eq!(parse_deep_link("weebo-notify://n"), None, "no id at all");
        assert_eq!(
            parse_deep_link("weebo-notify://n/a/b"),
            None,
            "two segments"
        );
        assert_eq!(parse_deep_link("not a url"), None);
    }

    #[test]
    fn an_id_cannot_carry_the_router_somewhere_else() {
        // Percent-encoded separators are the interesting case: they survive URL
        // parsing as literal characters and would be re-expanded by whatever builds
        // the next address.
        assert_eq!(parse_deep_link("weebo-notify://n/%2F..%2Fadmin"), None);
        assert_eq!(parse_deep_link("weebo-notify://n/%5Cadmin"), None);
    }

    #[test]
    fn the_scheme_is_matched_case_insensitively() {
        // RFC 3986 makes the scheme case-insensitive, and an OS is entitled to hand
        // it over in whatever case the registration used.
        assert_eq!(
            parse_deep_link("WEEBO-NOTIFY://c/ci-infra"),
            Some(DeepLink::Channel("ci-infra".to_owned()))
        );
    }
}
