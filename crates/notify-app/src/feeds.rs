//! Channels fed by RSS, Atom and JSON feeds (RFC 0002): the leader's poll pass.
//!
//! Each due feed is fetched, its new items worked out against the keys it has
//! already seen, and each one published through [`UpsertNotification`] as the
//! feed's service account, so authorisation, quotas and attribution are the
//! publish path's own. The first poll of a feed only records what is there.
//!
//! # What makes an item published exactly once
//!
//! The chosen items are saved as the feed's `batch` before the first publish, and
//! each success moves its key from `batch` to `seen` in one save conditional on
//! the row's version (decisions 8 and 11). A crash repeats at most the item in
//! flight, a refusal repeats nothing, and a pass that finds a batch publishes it
//! as stored before fetching anything.

use std::sync::Arc;

use futures::StreamExt as _;
use notify_domain::hygiene::{TextKind, sanitize};
use notify_domain::limits::MAX_TITLE_CHARS;
use notify_domain::{
    Action, Actions, ContentDraft, DomainError, ErrorKind, ExternalId, Principal, RevisionDraft,
};
use notify_ports::{
    Clock, DeclaredFeed, Failure, FeedFetcher, FeedKey, FeedLang, FeedRegistry, FeedRequest,
    FeedState, FeedStateStore, FetchError, Fetched, FetchedFeed, LeaderElection, LeaderGuard,
    Metrics, PendingItem,
};
use time::{Duration, OffsetDateTime};

use crate::authz::Caller;
use crate::publish::{UpsertNotification, UpsertRequest};

/// How many item keys a feed remembers: twice the adapter's 200-entry window, so
/// an item that leaves the window and comes back is not new again.
pub const SEEN_MAX: usize = 400;

/// Feeds fetched at once.
const CONCURRENCY: usize = 4;

/// How long a pass keeps starting feeds; the rest wait for the next pass. Below
/// `supervise`'s pass timeout, so a pass is never killed mid-save.
const PASS_BUDGET: std::time::Duration = std::time::Duration::from_secs(60);

/// The longest a feed waits between polls, backoff and `Retry-After` included.
const CEILING: Duration = Duration::hours(24);

/// How long the row of a feed no longer declared is kept (decision 9): a leader
/// still on the old config, or a feed taken out and put back, keeps its state.
const GRACE: Duration = Duration::hours(24);

/// Due rows read per pass.
const DUE_LIMIT: u32 = 1_000;

/// What one pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FeedReport {
    /// Whether this replica led.
    pub led: bool,
    /// Feeds fetched.
    pub polled: u64,
    /// Notifications published, summaries included.
    pub published: u64,
    /// Items folded into a summary.
    pub summarised: u64,
    /// Fetches that failed.
    pub failed: u64,
}

impl FeedReport {
    fn absorb(&mut self, other: Self) {
        self.polled = self.polled.saturating_add(other.polled);
        self.published = self.published.saturating_add(other.published);
        self.summarised = self.summarised.saturating_add(other.summarised);
        self.failed = self.failed.saturating_add(other.failed);
    }
}

/// The periodic feed poll (RFC 0002 §6.2 — leader-only).
#[derive(Clone)]
pub struct PollFeeds {
    registry: Arc<dyn FeedRegistry>,
    store: Arc<dyn FeedStateStore>,
    fetcher: Arc<dyn FeedFetcher>,
    upsert: UpsertNotification,
    election: Arc<dyn LeaderElection>,
    clock: Arc<dyn Clock>,
    metrics: Arc<dyn Metrics>,
}

impl core::fmt::Debug for PollFeeds {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PollFeeds").finish_non_exhaustive()
    }
}

/// How a feed's turn ended, for `notify_feed_polls_total{outcome}`.
type Outcome = &'static str;

/// Why publishing a batch stopped before its end.
enum Stopped {
    /// Quota or grant: the rest waits for the next poll.
    Refused,
    /// Another writer saved first, or the lock is gone.
    Superseded,
}

impl PollFeeds {
    /// The name this job takes its lock under.
    pub const JOB: &'static str = "feed-poll";

    #[must_use]
    pub fn new(
        registry: Arc<dyn FeedRegistry>,
        store: Arc<dyn FeedStateStore>,
        fetcher: Arc<dyn FeedFetcher>,
        upsert: UpsertNotification,
        election: Arc<dyn LeaderElection>,
        clock: Arc<dyn Clock>,
        metrics: Arc<dyn Metrics>,
    ) -> Self {
        Self {
            registry,
            store,
            fetcher,
            upsert,
            election,
            clock,
            metrics,
        }
    }

    /// Run one pass.
    pub async fn execute(&self) -> Result<FeedReport, Failure> {
        let Some(guard) = self.election.try_acquire(Self::JOB).await? else {
            return Ok(FeedReport::default());
        };
        let outcome = self.pass(guard.as_ref()).await;
        guard.release().await;
        outcome
    }

    async fn pass(&self, guard: &dyn LeaderGuard) -> Result<FeedReport, Failure> {
        let started = std::time::Instant::now();
        let now = self.clock.now();
        let declared: Vec<(FeedKey, DeclaredFeed)> = self
            .registry
            .feeds()
            .into_iter()
            .map(|feed| (feed.key(), feed))
            .collect();

        let fresh: Vec<FeedState> = declared
            .iter()
            .map(|(key, feed)| {
                let first = now.saturating_add(spread(key, feed.every));
                FeedState::fresh(key.clone(), feed.channel.clone(), first)
            })
            .collect();
        self.store.ensure(&fresh).await?;
        let keys: Vec<FeedKey> = declared.iter().map(|(key, _)| key.clone()).collect();
        self.store
            .retain(&keys, now, now.saturating_sub(GRACE))
            .await?;

        // Only declared feeds come due: the row of one this replica does not declare
        // is left alone, since during a rolling deploy another replica may.
        let by_key: std::collections::HashMap<&FeedKey, &DeclaredFeed> =
            declared.iter().map(|(key, feed)| (key, feed)).collect();
        let work: Vec<(DeclaredFeed, FeedState)> = self
            .store
            .due(&keys, now, DUE_LIMIT)
            .await?
            .into_iter()
            .filter_map(|state| Some(((*by_key.get(&state.key)?).clone(), state)))
            .collect();

        let outcomes: Vec<Result<FeedReport, Failure>> = futures::stream::iter(work)
            .map(|(feed, state)| self.feed(feed, state, guard, started))
            .buffer_unordered(CONCURRENCY)
            .collect()
            .await;
        let mut report = FeedReport {
            led: true,
            ..FeedReport::default()
        };
        // Every feed finishes before the first error is reported.
        for outcome in outcomes {
            report.absorb(outcome?);
        }

        self.metrics.feed_failing(self.store.failing().await?);
        if report.published > 0 {
            tracing::info!(
                published = report.published,
                summarised = report.summarised,
                "feed poll published"
            );
        }
        Ok(report)
    }

    /// One feed's turn: its stored batch, then a fetch and what it brings.
    async fn feed(
        &self,
        feed: DeclaredFeed,
        mut state: FeedState,
        guard: &dyn LeaderGuard,
        started: std::time::Instant,
    ) -> Result<FeedReport, Failure> {
        let mut report = FeedReport::default();
        if started.elapsed() > PASS_BUDGET || !guard.is_held().await {
            return Ok(report);
        }

        // A batch chosen earlier is published as it was chosen, before anything is
        // fetched, so a feed that changed since cannot shift it (decision 8).
        if let Err(stopped) = self.publish(&feed, &mut state, guard, &mut report).await? {
            self.metrics.feed_poll(stopped.outcome());
            return Ok(report);
        }

        report.polled = 1;
        let outcome = match self.poll(&feed, &mut state, &mut report).await? {
            Some(outcome) => outcome,
            None => match self.publish(&feed, &mut state, guard, &mut report).await? {
                Ok(()) => "ok",
                Err(stopped) => stopped.outcome(),
            },
        };
        self.metrics.feed_poll(outcome);
        Ok(report)
    }

    /// Fetch, and save what the fetch decided: the new `seen`, and the batch to
    /// publish. `None` when a batch is left to publish; otherwise the outcome.
    async fn poll(
        &self,
        feed: &DeclaredFeed,
        state: &mut FeedState,
        report: &mut FeedReport,
    ) -> Result<Option<Outcome>, Failure> {
        let now = self.clock.now();
        let request = FeedRequest {
            url: feed.url.clone(),
            etag: state.etag.clone(),
            last_modified: state.last_modified.clone(),
            allow_private_network: feed.allow_private_network,
        };
        let fetched = match self.fetcher.fetch(&request).await {
            Ok(fetched) => fetched,
            Err(error) => {
                tracing::warn!(
                    channel = %feed.channel,
                    feed = feed.position,
                    url = %feed.redacted,
                    %error,
                    "feed fetch failed"
                );
                report.failed = 1;
                let retry_after = match &error {
                    FetchError::Status { retry_after, .. } => *retry_after,
                    _ => None,
                };
                state.failures = state.failures.saturating_add(1);
                state.next_poll_at = next_poll(now, feed.every, state.failures, retry_after);
                return Ok(Some(self.save(state).await?.unwrap_or(error.outcome())));
            }
        };

        state.failures = 0;
        state.last_ok_at = Some(now);
        state.next_poll_at = next_poll(now, feed.every, 0, None);
        let fetched = match fetched {
            Fetched::NotModified => {
                return Ok(Some(self.save(state).await?.unwrap_or("not_modified")));
            }
            Fetched::Feed(fetched) => fetched,
        };
        if fetched.invalid > 0 {
            self.metrics.feed_items(
                "invalid",
                u64::try_from(fetched.invalid).unwrap_or(u64::MAX),
            );
        }
        state.etag.clone_from(&fetched.etag);
        state.last_modified.clone_from(&fetched.last_modified);

        let keys: Vec<String> = fetched
            .entries
            .iter()
            .map(|entry| state.key.item(&entry.id))
            .collect();
        let outcome = match plan(state.seen.as_deref(), &keys) {
            Plan::Prime => {
                tracing::info!(
                    channel = %feed.channel,
                    feed = feed.position,
                    items = keys.len(),
                    "feed primed"
                );
                remember(state.seen.get_or_insert_with(Vec::new), keys);
                "ok"
            }
            Plan::Suspicious => {
                tracing::warn!(
                    channel = %feed.channel,
                    feed = feed.position,
                    url = %feed.redacted,
                    items = keys.len(),
                    "every item of the feed is new: recorded, nothing published"
                );
                remember(state.seen.get_or_insert_with(Vec::new), keys);
                "suspicious"
            }
            Plan::New(new) => {
                let (batch, folded) = choose(feed, &state.key, &fetched, &keys, &new);
                let summarised = u64::try_from(folded.len()).unwrap_or(u64::MAX);
                remember(state.seen.get_or_insert_with(Vec::new), folded);
                state.batch = batch;
                if summarised > 0 {
                    report.summarised = summarised;
                    self.metrics.feed_items("summarised", summarised);
                }
                "ok"
            }
        };
        if let Some(superseded) = self.save(state).await? {
            return Ok(Some(superseded));
        }
        Ok(state.batch.is_empty().then_some(outcome))
    }

    /// Publish the stored batch one item at a time, saving after each.
    async fn publish(
        &self,
        feed: &DeclaredFeed,
        state: &mut FeedState,
        guard: &dyn LeaderGuard,
        report: &mut FeedReport,
    ) -> Result<Result<(), Stopped>, Failure> {
        let caller = Caller::member(Principal::ServiceAccount {
            id: feed.publisher.clone(),
        });
        while let Some(item) = state.batch.first().cloned() {
            if !guard.is_held().await {
                return Ok(Err(Stopped::Superseded));
            }
            match self.upsert_one(&caller, feed, &item).await {
                Ok(true) => {
                    report.published = report.published.saturating_add(1);
                    self.metrics.feed_items("published", 1);
                }
                Ok(false) => self.metrics.feed_items("invalid", 1),
                Err(Failure::Port(error)) => return Err(error.into()),
                Err(error) => {
                    tracing::warn!(
                        channel = %feed.channel,
                        feed = feed.position,
                        publisher = %feed.publisher,
                        %error,
                        "a feed item was refused; the rest waits for the next poll"
                    );
                    // Not a fetch failure: `failures` and the gauge are the fetch's.
                    // The quota refills within a minute, so the next try is `every` away.
                    state.next_poll_at = next_poll(self.clock.now(), feed.every, 0, None);
                    return Ok(Err(match self.save(state).await? {
                        Some(_) => Stopped::Superseded,
                        None => Stopped::Refused,
                    }));
                }
            }
            state.batch.remove(0);
            if let Some(key) = item.key {
                remember(state.seen.get_or_insert_with(Vec::new), [key]);
            }
            if self.save(state).await?.is_some() {
                return Ok(Err(Stopped::Superseded));
            }
        }
        Ok(Ok(()))
    }

    /// `Ok(true)` published, `Ok(false)` refused by the domain as an item and
    /// skipped (decision 12); `Err` for a refusal the operator can fix.
    async fn upsert_one(
        &self,
        caller: &Caller,
        feed: &DeclaredFeed,
        item: &PendingItem,
    ) -> Result<bool, Failure> {
        let invalid = |error: &DomainError| {
            tracing::warn!(
                channel = %feed.channel,
                feed = feed.position,
                %error,
                "a feed item was refused by the domain; skipped"
            );
            false
        };
        let external_id = match ExternalId::parse(&item.external_id) {
            Ok(id) => id,
            Err(error) => return Ok(invalid(&error)),
        };
        let request = UpsertRequest {
            channel: feed.channel.clone(),
            external_id,
            draft: draft_for(item, feed),
        };
        match self.upsert.execute(caller, request).await {
            // Already there, and now closed to revisions: it was published.
            Ok(_)
            | Err(Failure::Domain(
                DomainError::RevisionLimitReached { .. } | DomainError::NotificationGone,
            )) => Ok(true),
            Err(Failure::Domain(error))
                if matches!(error.kind(), ErrorKind::Validation | ErrorKind::TooLarge) =>
            {
                Ok(invalid(&error))
            }
            Err(error) => Err(error),
        }
    }

    /// Save, bumping the local version. `Some("superseded")` when another writer
    /// saved first: nothing was written, and this copy must stop.
    async fn save(&self, state: &mut FeedState) -> Result<Option<Outcome>, Failure> {
        if self.store.save(state).await? {
            state.version = state.version.saturating_add(1);
            return Ok(None);
        }
        tracing::warn!(
            feed = state.key.as_str(),
            "feed state saved by another writer; stopping"
        );
        Ok(Some("superseded"))
    }
}

impl Stopped {
    const fn outcome(&self) -> Outcome {
        match self {
            Self::Refused => "publish_refused",
            Self::Superseded => "superseded",
        }
    }
}

/// What a fetch means for a feed.
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    /// The first poll: record, publish nothing.
    Prime,
    /// Every item is new on a later poll: ids that churn, or a feed that outran its
    /// window. Record, publish nothing.
    Suspicious,
    /// The indices of the new entries, in feed order, each key once.
    New(Vec<usize>),
}

fn plan(seen: Option<&[String]>, keys: &[String]) -> Plan {
    let Some(seen) = seen else {
        return Plan::Prime;
    };
    let mut new: Vec<usize> = Vec::new();
    for (index, key) in keys.iter().enumerate() {
        let repeated = new
            .iter()
            .any(|&earlier| keys.get(earlier).is_some_and(|other| other == key));
        if !seen.contains(key) && !repeated {
            new.push(index);
        }
    }
    // A feed primed while empty has nothing to churn against: its first posts are new.
    if !seen.is_empty() && keys.len() > 1 && new.len() == keys.len() {
        return Plan::Suspicious;
    }
    Plan::New(new)
}

/// Append the keys not already there, then drop the oldest past [`SEEN_MAX`].
fn remember(seen: &mut Vec<String>, keys: impl IntoIterator<Item = String>) {
    for key in keys {
        if !seen.contains(&key) {
            seen.push(key);
        }
    }
    let excess = seen.len().saturating_sub(SEEN_MAX);
    seen.drain(..excess);
}

/// The batch to publish, oldest first, and the keys folded into its summary: past
/// `max_per_poll`, the newest are published and the older rest become one summary
/// published before them (decision 7).
fn choose(
    feed: &DeclaredFeed,
    key: &FeedKey,
    fetched: &FetchedFeed,
    keys: &[String],
    new: &[usize],
) -> (Vec<PendingItem>, Vec<String>) {
    let mut order = new.to_vec();
    // Undated items sort first; a feed lists newest first, so lower is older.
    order.sort_by_key(|&index| {
        let entry = fetched.entries.get(index);
        (
            entry.and_then(|entry| entry.published),
            entry.and_then(|entry| entry.updated),
            core::cmp::Reverse(index),
        )
    });
    let cut = order
        .len()
        .saturating_sub(usize::try_from(feed.max_per_poll).unwrap_or(usize::MAX));
    let (folding, newest) = order.split_at(cut);

    let folded: Vec<String> = folding
        .iter()
        .filter_map(|&index| keys.get(index).cloned())
        .collect();
    let mut batch = Vec::new();
    if !folded.is_empty() {
        let name = title_of([fetched.title.as_deref()], feed.url.host_str());
        batch.push(PendingItem {
            key: None,
            external_id: format!("feed:{}:more:{}", key.as_str(), key.folded(&folded)),
            title: cut_title(&summary_title(feed.lang, folded.len(), &name)),
            link: fetched.site.clone(),
        });
    }
    for &index in newest {
        let (Some(entry), Some(item)) = (fetched.entries.get(index), keys.get(index)) else {
            continue;
        };
        batch.push(PendingItem {
            key: Some(item.clone()),
            external_id: format!("feed:{item}"),
            title: title_of(
                [entry.title.as_deref(), fetched.title.as_deref()],
                feed.url.host_str(),
            ),
            link: entry.link.clone(),
        });
    }
    (batch, folded)
}

/// The first candidate that is not empty once sanitised as one line, else the
/// feed URL's host, cut to the title ceiling: a title always exists.
fn title_of<'a>(
    candidates: impl IntoIterator<Item = Option<&'a str>>,
    host: Option<&str>,
) -> String {
    let title = candidates
        .into_iter()
        .flatten()
        .map(|candidate| sanitize(candidate, TextKind::SingleLine))
        .find(|candidate| !candidate.is_empty())
        .unwrap_or_else(|| host.unwrap_or("feed").to_owned());
    cut_title(&title)
}

fn cut_title(title: &str) -> String {
    title.chars().take(MAX_TITLE_CHARS).collect()
}

/// `"<n> more items from <feed>"`, in the feed's language (decision 14).
fn summary_title(lang: FeedLang, count: usize, feed: &str) -> String {
    match (lang, count) {
        (FeedLang::En, 1) => format!("1 more item from {feed}"),
        (FeedLang::En, _) => format!("{count} more items from {feed}"),
        (FeedLang::Fr, 1) => format!("1 autre élément de {feed}"),
        (FeedLang::Fr, _) => format!("{count} autres éléments de {feed}"),
    }
}

pub(crate) const fn open_label(lang: FeedLang) -> &'static str {
    match lang {
        FeedLang::En => "Open",
        FeedLang::Fr => "Ouvrir",
    }
}

/// The revision an item becomes: its title, a *View* link, the feed's tags and the
/// feed key as group. Body, priority and TTL are left to the channel policy.
fn draft_for(item: &PendingItem, feed: &DeclaredFeed) -> RevisionDraft {
    let actions = item
        .link
        .clone()
        .and_then(|url| {
            Actions::new(vec![Action::View {
                label: open_label(feed.lang).to_owned(),
                url,
            }])
            .ok()
        })
        .unwrap_or_default();
    RevisionDraft::simple(ContentDraft {
        title: item.title.clone(),
        actions,
        tags: feed.tags.clone(),
        group_key: Some(feed.key().as_str().to_owned()),
        ..ContentDraft::default()
    })
}

/// When a feed is next due: `every` after a success; after a failure
/// `every × 2^failures`, or the host's `Retry-After` if longer, both at most a day.
#[must_use]
pub fn next_poll(
    now: OffsetDateTime,
    every: Duration,
    failures: u32,
    retry_after: Option<std::time::Duration>,
) -> OffsetDateTime {
    if failures == 0 {
        return now.saturating_add(every);
    }
    let factor = 1_i32
        .checked_shl(failures)
        .filter(|factor| *factor > 0)
        .unwrap_or(i32::MAX);
    let backoff = every.saturating_mul(factor).min(CEILING);
    let asked = retry_after
        .map(|asked| Duration::try_from(asked).unwrap_or(CEILING).min(CEILING))
        .unwrap_or_default();
    now.saturating_add(backoff.max(asked))
}

/// A feed's first poll is offset within `every` by its key, so feeds declared at
/// one boot are not polled in lockstep.
fn spread(key: &FeedKey, every: Duration) -> Duration {
    let seed = key
        .as_str()
        .get(..12)
        .and_then(|hex| u64::from_str_radix(hex, 16).ok())
        .unwrap_or(0);
    let span = u64::try_from(every.whole_seconds()).unwrap_or(0);
    let offset = seed.checked_rem(span).unwrap_or(0);
    Duration::seconds(i64::try_from(offset).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use notify_domain::{ChannelId, ServiceAccountId};
    use notify_ports::FeedEntry;

    use super::*;

    fn feed() -> DeclaredFeed {
        DeclaredFeed {
            channel: ChannelId::parse("veille").unwrap(),
            position: 0,
            url: "https://blog.example.org/feed.xml".parse().unwrap(),
            redacted: "https://blog.example.org#x".to_owned(),
            publisher: ServiceAccountId::parse("bot-feeds").unwrap(),
            every: Duration::minutes(30),
            tags: vec!["blog".to_owned()],
            max_per_poll: 2,
            allow_private_network: false,
            lang: FeedLang::En,
        }
    }

    fn entry(id: &str, day: Option<u8>) -> FeedEntry {
        FeedEntry {
            id: id.to_owned(),
            title: Some(format!("post {id}")),
            link: Some(format!("https://blog.example.org/{id}").parse().unwrap()),
            published: day.map(|day| {
                time::macros::datetime!(2026-10-01 0:00 UTC)
                    .saturating_add(Duration::days(i64::from(day)))
            }),
            updated: None,
        }
    }

    fn fetched(entries: Vec<FeedEntry>) -> FetchedFeed {
        FetchedFeed {
            title: Some("The Blog".to_owned()),
            site: Some("https://blog.example.org/".parse().unwrap()),
            entries,
            invalid: 0,
            etag: None,
            last_modified: None,
        }
    }

    fn keys(items: &[&str]) -> Vec<String> {
        items.iter().map(|&item| item.to_owned()).collect()
    }

    #[test]
    fn the_first_poll_primes_and_later_ones_find_what_is_new() {
        assert_eq!(plan(None, &keys(&["a", "b"])), Plan::Prime);
        let seen = keys(&["a", "b"]);
        assert_eq!(plan(Some(&seen), &keys(&["a", "b"])), Plan::New(vec![]));
        assert_eq!(
            plan(Some(&seen), &keys(&["c", "a", "b"])),
            Plan::New(vec![0])
        );
        assert_eq!(
            plan(Some(&seen), &keys(&["c", "c", "a"])),
            Plan::New(vec![0]),
            "a key listed twice is new once"
        );
        assert_eq!(
            plan(Some(&keys(&["a"])), &keys(&["b"])),
            Plan::New(vec![0]),
            "one item is not churn"
        );
    }

    #[test]
    fn an_all_new_later_fetch_is_suspicious() {
        let seen = keys(&["a", "b"]);
        assert_eq!(plan(Some(&seen), &keys(&["x", "y"])), Plan::Suspicious);
    }

    #[test]
    fn a_feed_primed_empty_publishes_its_first_posts() {
        assert_eq!(plan(Some(&[]), &keys(&["x", "y"])), Plan::New(vec![0, 1]));
    }

    #[test]
    fn seen_keeps_first_sight_order_and_drops_the_oldest_past_the_bound() {
        let mut seen = keys(&["a", "b"]);
        remember(&mut seen, keys(&["b", "c"]));
        assert_eq!(seen, keys(&["a", "b", "c"]));

        let mut full: Vec<String> = (0..SEEN_MAX).map(|n| n.to_string()).collect();
        remember(&mut full, keys(&["new"]));
        assert_eq!(full.len(), SEEN_MAX);
        assert_eq!(full.first().map(String::as_str), Some("1"));
        assert_eq!(full.last().map(String::as_str), Some("new"));
    }

    #[test]
    fn an_item_back_in_the_window_is_not_new_again() {
        // A CDN edge serving an older window flips `old` back in.
        let mut seen = None;
        let first = keys(&["new", "mid"]);
        assert_eq!(plan(seen.as_deref(), &first), Plan::Prime);
        remember(seen.get_or_insert_with(Vec::new), first);
        remember(seen.get_or_insert_with(Vec::new), keys(&["old"]));

        assert_eq!(
            plan(seen.as_deref(), &keys(&["mid", "old"])),
            Plan::New(vec![])
        );
    }

    #[test]
    fn new_items_publish_oldest_first_and_the_overflow_becomes_one_summary() {
        let feed = feed();
        let key = feed.key();
        let fetched = fetched(vec![
            entry("e", Some(5)),
            entry("d", Some(4)),
            entry("c", Some(3)),
            entry("b", None),
            entry("a", Some(1)),
        ]);
        let item_keys: Vec<String> = fetched.entries.iter().map(|e| key.item(&e.id)).collect();

        let (batch, folded) = choose(&feed, &key, &fetched, &item_keys, &[0, 1, 2, 3, 4]);

        let titles: Vec<&str> = batch.iter().map(|item| item.title.as_str()).collect();
        assert_eq!(titles, ["3 more items from The Blog", "post d", "post e"]);
        assert_eq!(folded.len(), 3);
        assert_eq!(batch[0].key, None);
        assert_eq!(batch[0].link, fetched.site);
        assert!(
            batch[0]
                .external_id
                .starts_with(&format!("feed:{}:more:", key.as_str()))
        );
        assert_eq!(batch[1].external_id, format!("feed:{}", item_keys[1]));

        let (again, _) = choose(&feed, &key, &fetched, &item_keys, &[4, 3, 2, 1, 0]);
        assert_eq!(
            again[0].external_id, batch[0].external_id,
            "a stable summary id"
        );
    }

    #[test]
    fn summaries_speak_the_feed_s_language_with_a_singular() {
        assert_eq!(summary_title(FeedLang::En, 1, "B"), "1 more item from B");
        assert_eq!(summary_title(FeedLang::Fr, 1, "B"), "1 autre élément de B");
        assert_eq!(
            summary_title(FeedLang::Fr, 4, "B"),
            "4 autres éléments de B"
        );
        assert_eq!(open_label(FeedLang::Fr), "Ouvrir");
    }

    #[test]
    fn a_title_falls_back_to_the_feed_then_the_host_and_is_cut() {
        let url = Some("blog.example.org");
        assert_eq!(title_of([Some("  \u{7}"), Some("Blog")], url), "Blog");
        assert_eq!(title_of([Some(""), None], url), "blog.example.org");
        assert_eq!(title_of([Some("a\r\nb\u{202e}c")], url).lines().count(), 1);
        let long = "x".repeat(MAX_TITLE_CHARS + 10);
        assert_eq!(
            title_of([Some(long.as_str())], url).chars().count(),
            MAX_TITLE_CHARS
        );
    }

    #[test]
    fn a_summary_without_a_feed_title_names_the_host() {
        let feed = feed();
        let key = feed.key();
        let mut fetched = fetched(vec![
            entry("c", Some(3)),
            entry("b", Some(2)),
            entry("a", Some(1)),
        ]);
        fetched.title = None;
        fetched.site = None;
        let item_keys: Vec<String> = fetched.entries.iter().map(|e| key.item(&e.id)).collect();

        let (batch, _) = choose(&feed, &key, &fetched, &item_keys, &[0, 1, 2]);

        assert_eq!(batch[0].title, "1 more item from blog.example.org");
        assert_eq!(batch[0].link, None);
        assert_eq!(batch[1].title, "post b");
    }

    #[test]
    fn a_draft_carries_a_view_link_tags_and_the_feed_as_group() {
        let feed = feed();
        let draft = draft_for(
            &PendingItem {
                key: Some("k".to_owned()),
                external_id: "feed:k".to_owned(),
                title: "post".to_owned(),
                link: Some("https://blog.example.org/post".parse().unwrap()),
            },
            &feed,
        );

        assert_eq!(draft.content.tags, ["blog"]);
        assert_eq!(
            draft.content.group_key.as_deref(),
            Some(feed.key().as_str())
        );
        assert_eq!(draft.content.body, None);
        assert_eq!(draft.priority, None);
        assert_eq!(draft.ttl, None);
        assert!(matches!(
            draft.content.actions.as_slice().first(),
            Some(Action::View { label, .. }) if label == "Open"
        ));
    }

    #[test]
    fn backoff_doubles_to_a_day_and_honours_a_longer_retry_after() {
        let now = time::macros::datetime!(2026-10-27 12:00 UTC);
        let every = Duration::minutes(30);

        assert_eq!(next_poll(now, every, 0, None), now + every);
        assert_eq!(next_poll(now, every, 1, None), now + Duration::hours(1));
        assert_eq!(next_poll(now, every, 3, None), now + Duration::hours(4));
        assert_eq!(next_poll(now, every, 7, None), now + CEILING);
        assert_eq!(
            next_poll(now, every, u32::MAX, None),
            now + CEILING,
            "saturating"
        );
        assert_eq!(
            next_poll(now, every, 1, Some(std::time::Duration::from_hours(3))),
            now + Duration::hours(3)
        );
        assert_eq!(
            next_poll(
                now,
                every,
                1,
                Some(std::time::Duration::from_secs(u64::MAX))
            ),
            now + CEILING,
            "clamped"
        );
        assert_eq!(
            next_poll(now, every, 0, Some(std::time::Duration::from_hours(3))),
            now + every,
            "a success ignores it"
        );
    }

    #[test]
    fn first_polls_are_spread_within_the_interval() {
        let every = Duration::minutes(30);
        let offsets: Vec<Duration> = ["a", "b", "c", "d"]
            .iter()
            .map(|url| {
                let mut feed = feed();
                feed.url = format!("https://example.org/{url}").parse().unwrap();
                spread(&feed.key(), every)
            })
            .collect();

        assert!(
            offsets
                .iter()
                .all(|offset| *offset >= Duration::ZERO && *offset < every)
        );
        assert!(offsets.windows(2).any(|pair| pair[0] != pair[1]));
    }
}
