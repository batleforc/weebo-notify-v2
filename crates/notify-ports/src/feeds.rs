//! Reading a channel's feeds (RFC 0002): fetching an RSS, Atom or JSON feed an
//! operator declared, already parsed, so no parser type crosses the port.
//! Implementations refuse non-public addresses unless the request opts out, and
//! bound every fetch in time, size and item count (RFC 0002 §7). The declared
//! feeds come from config ([`FeedRegistry`]), what was seen of each from
//! [`FeedStateStore`].

use std::time::Duration;

use notify_domain::{ChannelId, ServiceAccountId};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use time::OffsetDateTime;
use url::Url;

use crate::error::PortResult;

/// One fetch of one feed. The validators are the last answer's, echoed back so an
/// unchanged feed costs a `304`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedRequest {
    pub url: Url,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// Dial private, loopback and link-local addresses too: the operator's opt-in.
    pub allow_private_network: bool,
}

/// What a fetch found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetched {
    /// `304`: nothing changed since the validators sent.
    NotModified,
    Feed(FetchedFeed),
}

/// A parsed feed, cut to the first entries the adapter reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedFeed {
    /// The feed's own title, if it has one.
    pub title: Option<String>,
    /// The feed's first `http(s)` site link, if any.
    pub site: Option<Url>,
    /// In feed order.
    pub entries: Vec<FeedEntry>,
    /// Entries skipped for having no identity at all: no id, link or title.
    pub invalid: usize,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

/// One item, with what a notification needs from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedEntry {
    /// Stable across fetches: the `guid`/`id`, else the first link, else the title
    /// plus `published` (RFC 0002 decision 10). Never empty. Not yet hashed.
    pub id: String,
    pub title: Option<String>,
    /// The first `http(s)` link.
    pub link: Option<Url>,
    pub published: Option<OffsetDateTime>,
    pub updated: Option<OffsetDateTime>,
}

/// Why a fetch gave nothing to read. Each maps to one `outcome` label.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FetchError {
    /// The name, the literal or a redirect led to a non-public address.
    #[error("the feed resolves to no public address")]
    RefusedAddress,
    /// The body passed the size cap, counted after decompression.
    #[error("the feed is larger than the size cap")]
    TooLarge,
    #[error("the feed host did not answer in time")]
    Timeout,
    /// A status other than `2xx` or `304`; `retry_after` from a `429`/`503`.
    #[error("the feed host answered HTTP {status}")]
    Status {
        status: u16,
        retry_after: Option<Duration>,
    },
    /// DNS, TLS, a refused connection or a refused redirect. The text never
    /// carries the URL.
    #[error("the feed host could not be reached: {0}")]
    Unreachable(String),
    #[error("the body is not a feed")]
    Unparsable,
}

impl FetchError {
    /// The `outcome` label of `notify_feed_polls_total` (RFC 0002 §10.1).
    #[must_use]
    pub const fn outcome(&self) -> &'static str {
        match self {
            Self::RefusedAddress => "refused_address",
            Self::TooLarge => "too_large",
            Self::Timeout => "timeout",
            Self::Status { .. } | Self::Unreachable(_) => "http_error",
            Self::Unparsable => "unparsable",
        }
    }
}

/// Fetches and parses one feed.
#[async_trait::async_trait]
pub trait FeedFetcher: Send + Sync {
    async fn fetch(&self, request: &FeedRequest) -> Result<Fetched, FetchError>;
}

/// The language of the strings the server writes into a feed's notifications
/// (RFC 0002 decision 14).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FeedLang {
    #[default]
    En,
    Fr,
}

/// One `[[channels.managed.feeds]]` entry, as config declares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredFeed {
    pub channel: ChannelId,
    /// Its index in the channel's `feeds`, for logs (`feeds[1]`).
    pub position: usize,
    pub url: Url,
    /// The URL as logs may show it: [`crate::redact_url`]'s fingerprint.
    pub redacted: String,
    pub publisher: ServiceAccountId,
    pub every: time::Duration,
    pub tags: Vec<String>,
    pub max_per_poll: u32,
    pub allow_private_network: bool,
    pub lang: FeedLang,
}

impl DeclaredFeed {
    #[must_use]
    pub fn key(&self) -> FeedKey {
        FeedKey(digest(&[self.channel.as_str(), self.url.as_str()]))
    }
}

/// A feed's identity: an unkeyed digest of its channel and exact URL, so the
/// table never holds the URL and a fingerprint-key rotation leaves rows in place
/// (decision 9).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FeedKey(String);

impl FeedKey {
    /// A stored key, as read back.
    #[must_use]
    pub const fn from_stored(raw: String) -> Self {
        Self(raw)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// One item's key within this feed, from its [`FeedEntry::id`].
    #[must_use]
    pub fn item(&self, id: &str) -> String {
        digest(&[&self.0, id])
    }

    /// The key of a set of item keys, for a summary's external id.
    #[must_use]
    pub fn folded(&self, items: &[String]) -> String {
        let parts: Vec<&str> = std::iter::once(self.0.as_str())
            .chain(items.iter().map(String::as_str))
            .collect();
        digest(&parts)
    }
}

/// Hex digest characters kept: 128 bits, and `feed:<key>:more:<key>` stays within
/// an external id's 128 characters.
pub const FEED_KEY_HEX: usize = 32;

/// `hex(sha256(parts joined by NUL))`, cut to [`FEED_KEY_HEX`]. NUL separates so
/// two different splits of the same text never collide.
fn digest(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            hasher.update([0]);
        }
        hasher.update(part.as_bytes());
    }
    let mut hex = base16ct::lower::encode_string(&hasher.finalize());
    hex.truncate(FEED_KEY_HEX);
    hex
}

/// A stored `ETag` or `Last-Modified`, or `None` if it could not be sent back as a
/// header: visible ASCII, spaces and tabs only, so a CRLF is cleared, not echoed.
#[must_use]
pub fn clean_validator(value: Option<&str>) -> Option<String> {
    value
        .filter(|value| {
            value
                .bytes()
                .all(|b| b == b'\t' || (b' '..=b'~').contains(&b))
        })
        .map(str::to_owned)
}

/// What config declares. Synchronous, like [`crate::SinkRegistry`]: swapped on
/// `SIGHUP`, never read from anywhere else.
pub trait FeedRegistry: Send + Sync {
    /// Every declared feed, in config order.
    fn feeds(&self) -> Vec<DeclaredFeed>;
}

/// A notification chosen for publishing and not yet published (decision 8),
/// stored whole so a replay publishes exactly what was chosen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingItem {
    /// The item key that joins `seen` once published; `None` for a summary, whose
    /// folded keys were recorded when the batch was chosen.
    pub key: Option<String>,
    pub external_id: String,
    /// Final, already fallen back and cut.
    pub title: String,
    pub link: Option<Url>,
}

/// What the poller knows of one feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedState {
    pub key: FeedKey,
    pub channel: ChannelId,
    /// `None` until the first poll; item keys in first-sight order, bounded.
    pub seen: Option<Vec<String>>,
    /// Chosen and not yet published, in publish order.
    pub batch: Vec<PendingItem>,
    /// Bumped by every save; a save from a stale copy is refused (decision 11).
    pub version: i64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub failures: u32,
    pub next_poll_at: OffsetDateTime,
    pub last_ok_at: Option<OffsetDateTime>,
}

impl FeedState {
    /// A feed never polled, first due at `next_poll_at`.
    #[must_use]
    pub const fn fresh(key: FeedKey, channel: ChannelId, next_poll_at: OffsetDateTime) -> Self {
        Self {
            key,
            channel,
            seen: None,
            batch: Vec::new(),
            version: 0,
            etag: None,
            last_modified: None,
            failures: 0,
            next_poll_at,
            last_ok_at: None,
        }
    }
}

/// How stale `touched_at` may get before [`FeedStateStore::retain`] rewrites it.
pub const TOUCH_EVERY: time::Duration = time::Duration::hours(1);

/// The `feed_state` table, read and written by the leader only.
#[async_trait::async_trait]
pub trait FeedStateStore: Send + Sync {
    /// Insert the rows that do not exist yet; existing rows are left as they are,
    /// and a row whose channel does not exist is not inserted.
    async fn ensure(&self, fresh: &[FeedState]) -> PortResult<()>;

    /// Mark `declared` as seen at `now`, and delete the rows of every other feed
    /// untouched since before `forget_before` (decision 9). Returns how many went.
    /// A row touched within [`TOUCH_EVERY`] is not rewritten: the grace period is a
    /// day, so a write per feed per pass would only bloat the table.
    async fn retain(
        &self,
        declared: &[FeedKey],
        now: OffsetDateTime,
        forget_before: OffsetDateTime,
    ) -> PortResult<u64>;

    /// The rows of `declared` feeds due at `now`, soonest first. Rows of feeds no
    /// longer declared never come due again, so they must not take the `limit`.
    async fn due(
        &self,
        declared: &[FeedKey],
        now: OffsetDateTime,
        limit: u32,
    ) -> PortResult<Vec<FeedState>>;

    async fn get(&self, key: &FeedKey) -> PortResult<Option<FeedState>>;

    /// Write `state` if the stored version is still `state.version`, bumping it.
    /// `false`: another writer got there first, and nothing was written.
    async fn save(&self, state: &FeedState) -> PortResult<bool>;

    /// How many feeds are failing (`failures > 0`), for `notify_feed_failing`.
    async fn failing(&self) -> PortResult<u64>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_32_hex_and_split_unambiguously() {
        let feed = FeedKey(digest(&["ci", "https://example.org/feed"]));
        assert_eq!(feed.as_str().len(), FEED_KEY_HEX);
        assert!(feed.as_str().bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(digest(&["ab", "c"]), digest(&["a", "bc"]));
        assert_eq!(feed.item("1"), feed.item("1"));
        assert_ne!(feed.item("1"), feed.item("2"));
        let summary = format!(
            "feed:{}:more:{}",
            feed.as_str(),
            feed.folded(&[feed.item("1")])
        );
        assert!(summary.len() <= notify_domain::limits::MAX_EXTERNAL_ID_CHARS);
    }
}
