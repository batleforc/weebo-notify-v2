//! Fetches and parses the feeds a managed channel declares (RFC 0002 §6.5).
//!
//! The server dials a URL an operator named, on a timer, and parses what a third
//! party sent back, so every fetch is bounded: public addresses only (the
//! resolver, IP literals and every redirect hop), no proxy, 20s in all, 2 MiB of
//! *decompressed* body, at most 3 redirects and none from `https` to `http`, and
//! the first 200 entries. The parser is feed-rs without its `sanitize` feature: no
//! HTML from a feed is rendered, only titles and links are kept.

use std::time::Duration;

use futures::StreamExt as _;
use notify_domain::is_public_address;
use notify_ports::{FeedEntry, FeedFetcher, FeedRequest, FetchError, Fetched, FetchedFeed};
use notify_push_http::{NotPublic, PublicOnly, is_not_public, retry};
use reqwest::header::{self, HeaderValue};
use time::OffsetDateTime;
use url::Url;

/// The most body read from one fetch, counted after decompression.
pub const MAX_BODY: usize = 2 * 1024 * 1024;
/// Entries past this many are not read.
pub const MAX_ENTRIES: usize = 200;
/// Connect, redirects and the whole body included.
const TIMEOUT: Duration = Duration::from_secs(20);
const MAX_REDIRECTS: usize = 3;
const ACCEPT: &str = "application/atom+xml, application/rss+xml, application/feed+json, \
                      application/xml;q=0.9, application/json;q=0.9, text/xml;q=0.8, */*;q=0.5";

/// The [`FeedFetcher`] over HTTP.
#[derive(Debug, Clone)]
pub struct HttpFeeds {
    /// Through [`PublicOnly`]: the default.
    public: reqwest::Client,
    /// For the feeds an operator opted out with `allow_private_network`.
    private: reqwest::Client,
}

impl HttpFeeds {
    /// Fails only if an HTTP client (the TLS backend) cannot be built.
    pub fn new() -> reqwest::Result<Self> {
        Ok(Self {
            public: client(false)?,
            private: client(true)?,
        })
    }
}

fn client(private: bool) -> reqwest::Result<reqwest::Client> {
    let builder = reqwest::Client::builder()
        .timeout(TIMEOUT)
        // A proxy would resolve the name itself, and `PublicOnly` would never be
        // asked: `HTTPS_PROXY` in the environment must not be a way around it.
        .no_proxy()
        .redirect(reqwest::redirect::Policy::custom(
            move |attempt| match hop_refusal(attempt.previous(), attempt.url(), private) {
                Some(Refusal::Address) => attempt.error(NotPublic),
                Some(Refusal::Redirect(reason)) => attempt.error(RefusedRedirect(reason)),
                None => attempt.follow(),
            },
        ))
        .user_agent(concat!(
            "weebo-notify/",
            env!("CARGO_PKG_VERSION"),
            " (+feeds)"
        ));
    if private {
        builder.build()
    } else {
        builder.dns_resolver(PublicOnly).build()
    }
}

enum Refusal {
    Address,
    Redirect(&'static str),
}

/// Whether to refuse a redirect to `next`, `previous` being the URLs already
/// visited. reqwest does not resolve IP literals, so the resolver never sees one:
/// a literal is checked here.
fn hop_refusal(previous: &[Url], next: &Url, private: bool) -> Option<Refusal> {
    if previous.len() > MAX_REDIRECTS {
        return Some(Refusal::Redirect("too many redirects"));
    }
    if !matches!(next.scheme(), "http" | "https") {
        return Some(Refusal::Redirect(
            "a redirect to a scheme other than http(s)",
        ));
    }
    if previous.last().is_some_and(|from| from.scheme() == "https") && next.scheme() == "http" {
        return Some(Refusal::Redirect("a redirect from https to http"));
    }
    (!private && is_private_literal(next)).then_some(Refusal::Address)
}

fn is_private_literal(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => !is_public_address(ip.into()),
        Some(url::Host::Ipv6(ip)) => !is_public_address(ip.into()),
        _ => false,
    }
}

#[derive(Debug)]
struct RefusedRedirect(&'static str);

impl std::fmt::Display for RefusedRedirect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for RefusedRedirect {}

#[async_trait::async_trait]
impl FeedFetcher for HttpFeeds {
    async fn fetch(&self, request: &FeedRequest) -> Result<Fetched, FetchError> {
        let http = if request.allow_private_network {
            &self.private
        } else if is_private_literal(&request.url) {
            return Err(FetchError::RefusedAddress);
        } else {
            &self.public
        };

        let mut get = http.get(request.url.clone()).header(header::ACCEPT, ACCEPT);
        // Stored validators are opaque: one that is not a valid header (CRLF) is not sent.
        if let Some(etag) = valid_header(request.etag.as_deref()) {
            get = get.header(header::IF_NONE_MATCH, etag);
        }
        if let Some(since) = valid_header(request.last_modified.as_deref()) {
            get = get.header(header::IF_MODIFIED_SINCE, since);
        }

        let response = get.send().await.map_err(failure)?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(Fetched::NotModified);
        }
        if !status.is_success() {
            let retry_after = matches!(status.as_u16(), 429 | 503)
                .then(|| retry_after(header_text(response.headers(), header::RETRY_AFTER)))
                .flatten();
            return Err(FetchError::Status {
                status: status.as_u16(),
                retry_after,
            });
        }

        let etag = header_text(response.headers(), header::ETAG).map(str::to_owned);
        let last_modified =
            header_text(response.headers(), header::LAST_MODIFIED).map(str::to_owned);
        let body = read_capped(response).await?;
        let mut feed = parse(&body, &request.url)?;
        feed.etag = etag;
        feed.last_modified = last_modified;
        Ok(Fetched::Feed(feed))
    }
}

/// `Retry-After` in either form: delta-seconds, or an HTTP-date, as the time left
/// until then. A date already past asks for nothing.
fn retry_after(header: Option<&str>) -> Option<Duration> {
    retry::retry_after(header).or_else(|| {
        let at = OffsetDateTime::parse(
            header?.trim(),
            &time::format_description::well_known::Rfc2822,
        )
        .ok()?;
        Duration::try_from(at - OffsetDateTime::now_utc()).ok()
    })
}

fn valid_header(value: Option<&str>) -> Option<HeaderValue> {
    HeaderValue::from_str(value?).ok()
}

/// A header as text; `to_str` only accepts visible ASCII, so no CRLF is ever kept.
fn header_text(headers: &header::HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name)?.to_str().ok()
}

/// The body, refused past [`MAX_BODY`] while it is read: `Content-Length` is only an
/// early refusal, since with `gzip` the bytes that count are the inflated ones.
async fn read_capped(response: reqwest::Response) -> Result<Vec<u8>, FetchError> {
    if response
        .content_length()
        .is_some_and(|length| usize::try_from(length).map_or(true, |length| length > MAX_BODY))
    {
        return Err(FetchError::TooLarge);
    }
    let mut body = Vec::new();
    let mut chunks = response.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(failure)?;
        if body.len().saturating_add(chunk.len()) > MAX_BODY {
            return Err(FetchError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// `without_url`: the URL may carry a feed token, and this text is logged.
fn failure(error: reqwest::Error) -> FetchError {
    let error = error.without_url();
    if is_not_public(&error) {
        return FetchError::RefusedAddress;
    }
    if error.is_timeout() {
        return FetchError::Timeout;
    }
    if error.is_decode() {
        return FetchError::Unparsable;
    }
    let reason = std::iter::successors(Some(&error as &dyn std::error::Error), |error| {
        error.source()
    })
    .map(ToString::to_string)
    .collect::<Vec<_>>()
    .join(": ");
    FetchError::Unreachable(reason)
}

/// Parse a feed body: RSS 0.9x/1.0/2.0, Atom or JSON Feed. `base` resolves
/// relative links. The validators are left empty for the caller.
pub fn parse(body: &[u8], base: &Url) -> Result<FetchedFeed, FetchError> {
    let feed = feed_rs::parser::Builder::new()
        .base_uri(Some(base.as_str()))
        .sanitize_content(false)
        // Ours below (decision 10): feed-rs's own fallback ends in a random UUID,
        // which would make every fetch of an id-less feed all new.
        .id_generator(|_, _, _| String::new())
        .build()
        .parse(body)
        .map_err(|_| FetchError::Unparsable)?;

    let mut entries = Vec::new();
    let mut invalid = 0_usize;
    for entry in feed.entries.into_iter().take(MAX_ENTRIES) {
        match entry_of(entry, base) {
            Some(entry) => entries.push(entry),
            None => invalid = invalid.saturating_add(1),
        }
    }
    Ok(FetchedFeed {
        title: feed.title.and_then(|title| non_blank(&title.content)),
        site: first_http(&feed.links, base),
        entries,
        invalid,
        etag: None,
        last_modified: None,
    })
}

fn entry_of(entry: feed_rs::model::Entry, base: &Url) -> Option<FeedEntry> {
    let title = entry.title.and_then(|title| non_blank(&title.content));
    let published = entry.published.and_then(|at| at_time(at.timestamp()));
    let link = first_http(&entry.links, base);
    // The link as resolved, not its raw text: whitespace or a relative form that
    // a CMS reformats would otherwise give every item a new identity.
    let id = non_blank(&entry.id)
        .or_else(|| link.as_ref().map(ToString::to_string))
        .or_else(|| {
            let title = title.as_deref()?;
            Some(published.map_or_else(
                || title.to_owned(),
                |at| format!("{title}\u{1f}{}", at.unix_timestamp()),
            ))
        })?;
    Some(FeedEntry {
        id,
        link,
        title,
        published,
        updated: entry.updated.and_then(|at| at_time(at.timestamp())),
    })
}

/// The first `http(s)` link to the page itself (`rel` absent or `alternate`): not
/// the feed's own `self`, nor an `enclosure`, `replies` or `edit` link.
fn first_http(links: &[feed_rs::model::Link], base: &Url) -> Option<Url> {
    links
        .iter()
        .filter(|link| matches!(link.rel.as_deref(), None | Some("alternate")))
        .filter_map(|link| base.join(link.href.trim()).ok())
        .find(|url| matches!(url.scheme(), "http" | "https"))
}

fn non_blank(text: &str) -> Option<String> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn at_time(unix: i64) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp(unix).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;

    fn base() -> Url {
        Url::parse("https://example.org/feed.xml").unwrap()
    }

    fn url(raw: &str) -> Url {
        Url::parse(raw).unwrap()
    }

    #[test]
    fn atom_ids_titles_and_links_are_read() {
        let feed = parse(
            br#"<?xml version="1.0"?>
            <feed xmlns="http://www.w3.org/2005/Atom">
              <title>Releases</title>
              <link rel="self" href="https://example.org/feed.xml"/>
              <link rel="alternate" href="/releases"/>
              <entry><id>tag:example.org,1</id><title>v1</title>
                <link href="https://example.org/v1"/><published>2026-10-01T00:00:00Z</published></entry>
            </feed>"#,
            &base(),
        )
        .unwrap();

        assert_eq!(feed.title.as_deref(), Some("Releases"));
        assert_eq!(feed.site, Some(url("https://example.org/releases")));
        let [entry] = feed.entries.as_slice() else {
            panic!("one entry")
        };
        assert_eq!(entry.id, "tag:example.org,1");
        assert_eq!(entry.title.as_deref(), Some("v1"));
        assert_eq!(entry.link, Some(url("https://example.org/v1")));
        assert!(entry.published.is_some());
    }

    #[test]
    fn an_rss_item_without_guid_is_keyed_on_its_link_whatever_its_title() {
        let rss = |title: &str| {
            format!(
                r#"<rss version="2.0"><channel><title>Blog</title>
                <item><title>{title}</title><link>https://example.org/post</link></item>
                </channel></rss>"#
            )
        };
        let before = parse(rss("Old title").as_bytes(), &base()).unwrap();
        let after = parse(rss("New title").as_bytes(), &base()).unwrap();

        assert_eq!(before.entries[0].id, "https://example.org/post");
        assert_eq!(before.entries[0].id, after.entries[0].id);
    }

    #[test]
    fn an_item_with_only_a_title_is_keyed_stably_and_one_with_nothing_is_invalid() {
        let rss = br#"<rss version="2.0"><channel><title>Blog</title>
            <item><title>Only a title</title><pubDate>Tue, 27 Oct 2026 10:00:00 GMT</pubDate></item>
            <item><description>nothing to key on</description></item>
            </channel></rss>"#;

        let first = parse(rss, &base()).unwrap();
        let second = parse(rss, &base()).unwrap();

        assert_eq!(first.entries.len(), 1);
        assert_eq!(first.invalid, 1);
        assert!(first.entries[0].id.starts_with("Only a title\u{1f}"));
        assert_eq!(first.entries[0].id, second.entries[0].id);
    }

    #[test]
    fn a_non_http_link_is_no_link() {
        let feed = parse(
            br#"<rss version="2.0"><channel><title>x</title>
            <item><guid>1</guid><title>t</title><link>javascript:alert(1)</link></item>
            </channel></rss>"#,
            &base(),
        )
        .unwrap();

        assert_eq!(feed.entries[0].link, None);
    }

    #[test]
    fn json_feed_parses() {
        let feed = parse(
            br#"{"version":"https://jsonfeed.org/version/1.1","title":"J",
                "items":[{"id":"a","title":"A","url":"https://example.org/a"}]}"#,
            &base(),
        )
        .unwrap();

        assert_eq!(feed.entries[0].id, "a");
        assert_eq!(feed.entries[0].link, Some(url("https://example.org/a")));
    }

    #[test]
    fn entries_past_the_cap_are_not_read() {
        let items = (0..1000).fold(String::new(), |mut items, n| {
            write!(items, "<item><guid>{n}</guid><title>{n}</title></item>").unwrap();
            items
        });
        let rss = format!(r#"<rss version="2.0"><channel><title>x</title>{items}</channel></rss>"#);

        let feed = parse(rss.as_bytes(), &base()).unwrap();

        assert_eq!(feed.entries.len(), MAX_ENTRIES);
    }

    #[test]
    fn garbage_is_unparsable() {
        assert_eq!(
            parse(b"<html><body>not a feed</body></html>", &base()),
            Err(FetchError::Unparsable)
        );
        assert_eq!(parse(b"", &base()), Err(FetchError::Unparsable));
    }

    #[test]
    fn a_billion_laughs_document_is_not_expanded() {
        let lol = br#"<?xml version="1.0"?>
            <!DOCTYPE lolz [<!ENTITY lol "lol"><!ENTITY lol2 "&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;">
            <!ENTITY lol3 "&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;">
            <!ENTITY lol4 "&lol3;&lol3;&lol3;&lol3;&lol3;&lol3;&lol3;&lol3;&lol3;&lol3;">]>
            <rss version="2.0"><channel><title>&lol4;</title>
            <item><guid>1</guid><title>&lol4;</title></item></channel></rss>"#;

        let started = std::time::Instant::now();
        let outcome = parse(lol, &base());

        assert!(started.elapsed() < Duration::from_secs(1));
        if let Ok(feed) = outcome {
            let title = feed.entries[0].title.clone().unwrap_or_default();
            assert!(
                title.len() < 100,
                "an entity was expanded: {} bytes",
                title.len()
            );
        }
    }

    #[test]
    fn an_item_without_guid_is_keyed_on_its_resolved_page_link() {
        let rss = |link: &str| {
            format!(
                r#"<feed xmlns="http://www.w3.org/2005/Atom"><title>x</title><entry>
                <title>t</title>
                <link rel="enclosure" href="https://cdn.example.org/a.mp3"/>
                <link href="{link}"/></entry></feed>"#
            )
        };
        let relative = parse(rss("  /post  ").as_bytes(), &base()).unwrap();
        let absolute = parse(rss("https://example.org/post").as_bytes(), &base()).unwrap();

        assert_eq!(relative.entries[0].id, "https://example.org/post");
        assert_eq!(
            relative.entries[0].id, absolute.entries[0].id,
            "same page, same key"
        );
        assert_eq!(
            relative.entries[0].link,
            Some(url("https://example.org/post"))
        );
    }

    #[test]
    fn retry_after_reads_seconds_and_http_dates() {
        assert_eq!(retry_after(Some("120")), Some(Duration::from_secs(120)));
        let later = (OffsetDateTime::now_utc() + time::Duration::hours(2))
            .format(&time::format_description::well_known::Rfc2822)
            .unwrap()
            .replace("+0000", "GMT");
        let asked = retry_after(Some(&later)).unwrap();
        assert!(
            asked > Duration::from_secs(7_100) && asked <= Duration::from_secs(7_200),
            "{asked:?}"
        );
        assert_eq!(
            retry_after(Some("Wed, 21 Oct 2015 07:28:00 GMT")),
            None,
            "past"
        );
        assert_eq!(retry_after(Some("soon")), None);
    }

    #[test]
    fn redirects_are_bounded_and_never_downgrade_or_reach_a_private_literal() {
        let from = [url("https://example.org/a")];
        let refused = |previous: &[Url], next: &str, private| {
            hop_refusal(previous, &url(next), private).is_some()
        };

        assert!(!refused(&from, "https://example.org/b", false));
        assert!(
            refused(&from, "http://example.org/b", false),
            "https → http"
        );
        assert!(refused(&from, "ftp://example.org/b", false));
        assert!(matches!(
            hop_refusal(&from, &url("http://127.0.0.1/feed"), false),
            Some(Refusal::Redirect(_))
        ));
        let plain = [url("http://example.org/a")];
        assert!(matches!(
            hop_refusal(&plain, &url("http://169.254.169.254/latest"), false),
            Some(Refusal::Address)
        ));
        assert!(refused(&plain, "http://[::1]/feed", false));
        assert!(!refused(&plain, "http://10.0.0.1/feed", true), "the opt-in");
        let three = [plain[0].clone(), plain[0].clone(), plain[0].clone()];
        assert!(!refused(&three, "http://example.org/c", false));
        let four = [three.as_slice(), &plain].concat();
        assert!(refused(&four, "http://example.org/c", false));
    }
}
