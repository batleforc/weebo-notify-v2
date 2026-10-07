//! `weebo-notify://n/<id>` and `://c/<id>` links, mirroring the web permalinks (§10bis.3quater).
//! They come from outside the app, so anything not exactly one of those shapes is refused.

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
    /// The web permalink path this link mirrors, so the shell routes like the Vue router.
    #[must_use]
    pub fn path(&self) -> String {
        match self {
            Self::Notification(id) => format!("/n/{id}"),
            Self::Channel(id) => format!("/c/{id}"),
        }
    }
}

/// Read a link the OS handed over; `None` for anything not exactly one of the two shapes.
#[must_use]
pub fn parse_deep_link(raw: &str) -> Option<DeepLink> {
    let url = Url::parse(raw).ok()?;

    // `Url` already lowercases the (case-insensitive) scheme.
    if url.scheme() != SCHEME {
        return None;
    }

    // `n` sits in the host position, not the path.
    let kind = url.host_str()?;

    let mut segments = url.path_segments()?;
    let id = segments.next()?;
    // Exactly one segment; a trailing slash (an empty one) is allowed.
    if segments.any(|extra| !extra.is_empty()) {
        return None;
    }

    // An allowlist: `path_segments` stays percent-encoded, so `%2F..%2Fadmin` has no literal `/`.
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
        // Percent-encoded separators survive parsing and could be re-expanded downstream.
        assert_eq!(parse_deep_link("weebo-notify://n/%2F..%2Fadmin"), None);
        assert_eq!(parse_deep_link("weebo-notify://n/%5Cadmin"), None);
    }

    #[test]
    fn the_scheme_is_matched_case_insensitively() {
        assert_eq!(
            parse_deep_link("WEEBO-NOTIFY://c/ci-infra"),
            Some(DeepLink::Channel("ci-infra".to_owned()))
        );
    }
}
