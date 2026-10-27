//! RFC 0002 §6.P 1, 2 and 4 and the §14 integration cases: [`PollFeeds`] against
//! the in-memory store and a fake fetcher serving the documents a relay would.

#![expect(
    clippy::unwrap_used,
    clippy::arithmetic_side_effects,
    reason = "a failing assertion is a failing test; counters on small, known values"
)]

use std::sync::{Arc, Mutex};

use notify_app::{Attribution, Authorizer, Fanout, PollFeeds, UpsertNotification};
use notify_domain::{
    Action, Channel, ChannelId, ExternalId, Membership, Origin, Principal, Scope, ServiceAccount,
    ServiceAccountId,
};
use notify_ports::test_utils::{InMemory, TestClock, TestLeaderElection, TestRateLimiter};
use notify_ports::{
    Allowance, Clock as _, DeclaredFeed, FeedEntry, FeedFetcher, FeedKey, FeedLang, FeedRegistry,
    FeedRequest, FeedState, FeedStateStore, FetchError, Fetched, FetchedFeed, NoMetrics,
    NotificationRepository as _, PortError, PortResult, Quota, RateLimiter,
};
use time::{Duration, OffsetDateTime};

fn channel() -> ChannelId {
    ChannelId::parse("veille-rust").unwrap()
}

fn publisher() -> ServiceAccountId {
    ServiceAccountId::parse("bot-feeds").unwrap()
}

fn declared(max_per_poll: u32) -> DeclaredFeed {
    DeclaredFeed {
        channel: channel(),
        position: 0,
        url: "https://blog.example.org/feed.xml".parse().unwrap(),
        redacted: "https://blog.example.org#0".to_owned(),
        publisher: publisher(),
        every: Duration::minutes(30),
        tags: vec!["blog".to_owned()],
        max_per_poll,
        allow_private_network: false,
        lang: FeedLang::En,
    }
}

struct Registry(DeclaredFeed);

impl FeedRegistry for Registry {
    fn feeds(&self) -> Vec<DeclaredFeed> {
        vec![self.0.clone()]
    }
}

/// Serves whatever the test last put in it, and counts the fetches.
#[derive(Clone, Default)]
struct Relay {
    feed: Arc<Mutex<Option<Result<Fetched, FetchError>>>>,
    requests: Arc<Mutex<Vec<FeedRequest>>>,
}

impl Relay {
    fn serve(&self, entries: &[(&str, u8)]) {
        let entries = entries
            .iter()
            .map(|&(id, day)| FeedEntry {
                id: id.to_owned(),
                title: Some(format!("post {id}")),
                link: Some(format!("https://blog.example.org/{id}").parse().unwrap()),
                published: Some(
                    time::macros::datetime!(2026-10-01 0:00 UTC) + Duration::days(day.into()),
                ),
                updated: None,
            })
            .collect();
        *self.feed.lock().unwrap() = Some(Ok(Fetched::Feed(FetchedFeed {
            title: Some("The Blog".to_owned()),
            site: Some("https://blog.example.org/".parse().unwrap()),
            entries,
            invalid: 0,
            etag: Some("W/\"1\"".to_owned()),
            last_modified: None,
        })));
    }

    fn answer(&self, answer: Result<Fetched, FetchError>) {
        *self.feed.lock().unwrap() = Some(answer);
    }

    fn fetches(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

#[async_trait::async_trait]
impl FeedFetcher for Relay {
    async fn fetch(&self, request: &FeedRequest) -> Result<Fetched, FetchError> {
        self.requests.lock().unwrap().push(request.clone());
        self.feed
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(Err(FetchError::Unparsable))
    }
}

/// Allows `per_pass` publishes of the channel, until `refill`.
#[derive(Clone)]
struct Budget {
    left: Arc<Mutex<u32>>,
    per_pass: u32,
}

impl Budget {
    fn refill(&self) {
        *self.left.lock().unwrap() = self.per_pass;
    }
}

#[async_trait::async_trait]
impl RateLimiter for Budget {
    async fn check(
        &self,
        quota: &Quota,
        _configured: Option<u32>,
        _now: OffsetDateTime,
    ) -> PortResult<Allowance> {
        if !matches!(quota, Quota::PublishPerChannel { .. }) {
            return Ok(Allowance::Allowed { remaining: 59 });
        }
        let mut left = self.left.lock().unwrap();
        if *left == 0 {
            return Ok(Allowance::Limited {
                retry_after_seconds: 60,
            });
        }
        *left -= 1;
        Ok(Allowance::Allowed { remaining: *left })
    }
}

/// A store whose saves fail from the `n`-th on: the process dying mid-batch.
#[derive(Clone)]
struct Crashing {
    inner: InMemory,
    saves_left: Arc<Mutex<Option<u32>>>,
}

#[async_trait::async_trait]
impl FeedStateStore for Crashing {
    async fn ensure(&self, fresh: &[FeedState]) -> PortResult<()> {
        self.inner.ensure(fresh).await
    }
    async fn retain(
        &self,
        declared: &[FeedKey],
        now: OffsetDateTime,
        forget_before: OffsetDateTime,
    ) -> PortResult<u64> {
        self.inner.retain(declared, now, forget_before).await
    }
    async fn due(&self, now: OffsetDateTime, limit: u32) -> PortResult<Vec<FeedState>> {
        self.inner.due(now, limit).await
    }
    async fn get(&self, key: &FeedKey) -> PortResult<Option<FeedState>> {
        FeedStateStore::get(&self.inner, key).await
    }
    async fn save(&self, state: &FeedState) -> PortResult<bool> {
        if let Some(left) = self.saves_left.lock().unwrap().as_mut() {
            if *left == 0 {
                return Err(PortError::corrupt("feed_state", "the process died"));
            }
            *left -= 1;
        }
        self.inner.save(state).await
    }
    async fn failing(&self) -> PortResult<u64> {
        self.inner.failing().await
    }
}

struct Fixture {
    store: InMemory,
    clock: TestClock,
    relay: Relay,
    poll: PollFeeds,
    feed: DeclaredFeed,
}

fn fixture(max_per_poll: u32, limiter: Arc<dyn RateLimiter>) -> Fixture {
    let clock = TestClock::new();
    let store = InMemory::with_clock(clock.clone());
    let mut seeded = Channel::dynamic(channel(), None).unwrap();
    seeded.upsert_member(
        Membership::new(
            Principal::ServiceAccount { id: publisher() },
            vec![Scope::Publish],
        )
        .unwrap(),
    );
    store.seed_channel(seeded);
    futures::executor::block_on(notify_ports::ServiceAccountRepository::upsert(
        &store,
        &ServiceAccount::new(
            publisher(),
            Some("Flux RSS"),
            None,
            Origin::Managed,
            None,
            clock.now(),
        )
        .unwrap(),
    ))
    .unwrap();

    let fanout = Fanout::new(
        Arc::new(store.clone()),
        Arc::new(store.clone()),
        Arc::new(store.clone()),
        Arc::new(store.clone()),
        Arc::new(store.clone()),
        Arc::new(clock.clone()),
        4_096,
    );
    let upsert = UpsertNotification::new(
        Arc::new(store.clone()),
        Arc::new(store.clone()),
        Arc::new(store.clone()),
        limiter,
        Arc::new(clock.clone()),
        Authorizer::new(Arc::new(store.clone())),
        fanout,
        Attribution::new(Arc::new(store.clone()), Arc::new(store.clone())),
    );
    let relay = Relay::default();
    let feed = declared(max_per_poll);
    let poll = PollFeeds::new(
        Arc::new(Registry(feed.clone())),
        Arc::new(store.clone()),
        Arc::new(relay.clone()),
        upsert,
        Arc::new(TestLeaderElection::leader()),
        Arc::new(clock.clone()),
        Arc::new(NoMetrics),
    );
    Fixture {
        store,
        clock,
        relay,
        poll,
        feed,
    }
}

fn simple(max_per_poll: u32) -> Fixture {
    fixture(max_per_poll, Arc::new(TestRateLimiter::new()))
}

impl Fixture {
    /// Let the feed come due, then run a pass. The very first pass only creates
    /// the feed's row, due within `every`, so it is run beforehand.
    async fn pass(&self) -> notify_app::FeedReport {
        if FeedStateStore::get(&self.store, &self.feed.key())
            .await
            .unwrap()
            .is_none()
        {
            assert_eq!(self.poll.execute().await.unwrap().polled, 0);
        }
        self.clock.advance(Duration::days(1));
        self.poll.execute().await.unwrap()
    }

    async fn state(&self) -> FeedState {
        FeedStateStore::get(&self.store, &self.feed.key())
            .await
            .unwrap()
            .unwrap()
    }

    async fn notification(&self, entry: &str) -> Option<notify_domain::Notification> {
        let key = self.feed.key().item(entry);
        self.store
            .find_by_external_id(
                &channel(),
                &ExternalId::parse(&format!("feed:{key}")).unwrap(),
            )
            .await
            .unwrap()
    }

    fn revisions(&self) -> usize {
        self.store
            .events()
            .iter()
            .filter(|event| matches!(event.payload, notify_ports::EventPayload::Revision(_)))
            .count()
    }
}

#[tokio::test]
async fn the_first_poll_records_the_feed_and_publishes_nothing() {
    let fixture = simple(10);
    fixture.relay.serve(&[("c", 3), ("b", 2), ("a", 1)]);

    let report = fixture.pass().await;

    assert_eq!((report.polled, report.published), (1, 0));
    assert_eq!(fixture.state().await.seen.map(|seen| seen.len()), Some(3));
    assert_eq!(fixture.revisions(), 0);
}

#[tokio::test]
async fn a_new_item_becomes_one_notification_by_the_publisher_with_a_view_link() {
    let fixture = simple(10);
    fixture.relay.serve(&[("c", 3), ("b", 2), ("a", 1)]);
    fixture.pass().await;
    fixture
        .relay
        .serve(&[("d", 4), ("c", 3), ("b", 2), ("a", 1)]);

    let report = fixture.pass().await;

    assert_eq!(report.published, 1);
    assert_eq!(fixture.revisions(), 1);
    let notification = fixture.notification("d").await.unwrap();
    assert_eq!(
        notification.head.author.display_name.as_deref(),
        Some("Flux RSS")
    );
    assert_eq!(notification.head.content.title, "post d");
    assert!(matches!(
        notification.head.content.actions.as_slice(),
        [Action::View { url, .. }] if url.as_str() == "https://blog.example.org/d"
    ));
    assert_eq!(notification.head.content.tags, ["blog"]);
    assert_eq!(fixture.state().await.batch.len(), 0);
    assert_eq!(
        fixture.relay.requests.lock().unwrap()[1].etag.as_deref(),
        Some("W/\"1\""),
        "the validator is sent back"
    );
}

#[tokio::test]
async fn an_item_still_in_the_feed_after_its_notification_is_gone_is_not_published_again() {
    let fixture = simple(10);
    fixture.relay.serve(&[("a", 1)]);
    fixture.pass().await;
    fixture.relay.serve(&[("b", 2), ("a", 1)]);
    fixture.pass().await;
    let published = fixture.notification("b").await.unwrap();
    fixture
        .store
        .mark_deleted(published.id, fixture.clock.now())
        .await
        .unwrap();

    let report = fixture.pass().await;

    assert_eq!(report.published, 0);
    assert_eq!(fixture.revisions(), 1);
}

#[tokio::test]
async fn an_unchanged_feed_publishes_nothing_and_a_failure_backs_off() {
    let fixture = simple(10);
    fixture.relay.serve(&[("a", 1)]);
    fixture.pass().await;

    fixture.relay.answer(Ok(Fetched::NotModified));
    assert_eq!(fixture.pass().await.published, 0);
    assert_eq!(fixture.state().await.failures, 0);

    fixture.relay.answer(Err(FetchError::Timeout));
    let report = fixture.pass().await;
    let state = fixture.state().await;
    assert_eq!((report.failed, state.failures), (1, 1));
    assert_eq!(state.next_poll_at, fixture.clock.now() + Duration::hours(1));
    assert_eq!(state.seen.map(|seen| seen.len()), Some(1), "seen is kept");
}

#[tokio::test]
async fn an_overflow_publishes_the_newest_and_one_summary_of_the_rest() {
    let fixture = simple(2);
    fixture.relay.serve(&[("a", 1), ("z", 0)]);
    fixture.pass().await;
    fixture
        .relay
        .serve(&[("f", 6), ("e", 5), ("d", 4), ("c", 3), ("b", 2), ("a", 1)]);

    let report = fixture.pass().await;

    assert_eq!((report.published, report.summarised), (3, 3));
    assert!(fixture.notification("f").await.is_some());
    assert!(fixture.notification("e").await.is_some());
    assert!(fixture.notification("d").await.is_none(), "folded");
    assert_eq!(fixture.state().await.seen.map(|seen| seen.len()), Some(7));
}

#[tokio::test]
async fn a_tight_quota_drains_the_backlog_over_passes_each_item_once() {
    let budget = Budget {
        left: Arc::new(Mutex::new(0)),
        per_pass: 3,
    };
    let fixture = fixture(10, Arc::new(budget.clone()));
    fixture.relay.serve(&[("old", 0), ("older", 0)]);
    fixture.pass().await;
    let items: Vec<(String, u8)> = (1..=11).map(|n| (format!("n{n:02}"), n)).collect();
    let mut served: Vec<(&str, u8)> = items.iter().map(|(id, day)| (id.as_str(), *day)).collect();
    served.push(("old", 0));
    fixture.relay.serve(&served);

    let mut published = 0;
    for _ in 0..6 {
        budget.refill();
        published += fixture.pass().await.published;
    }

    // Ten newest and one summary of the oldest, each upserted once.
    assert_eq!(published, 11);
    assert_eq!(fixture.revisions(), 11);
    assert_eq!(fixture.state().await.batch.len(), 0);
}

#[tokio::test]
async fn a_crash_mid_batch_replays_the_stored_batch_and_the_folded_set_does_not_shift() {
    let saves = Arc::new(Mutex::new(None));
    // The same fixture, its poller rebuilt over a store whose saves can fail.
    let fixture = {
        let fixture = simple(2);
        let crashing = Crashing {
            inner: fixture.store.clone(),
            saves_left: saves.clone(),
        };
        Fixture {
            poll: PollFeeds::new(
                Arc::new(Registry(fixture.feed.clone())),
                Arc::new(crashing),
                Arc::new(fixture.relay.clone()),
                upsert_of(&fixture),
                Arc::new(TestLeaderElection::leader()),
                Arc::new(fixture.clock.clone()),
                Arc::new(NoMetrics),
            ),
            ..fixture
        }
    };
    fixture.relay.serve(&[("a", 1), ("z", 0)]);
    fixture.pass().await;
    fixture
        .relay
        .serve(&[("e", 5), ("d", 4), ("c", 3), ("b", 2), ("a", 1)]);

    // The batch save, the summary and item `d` are saved; `e` dies after its publish.
    *saves.lock().unwrap() = Some(3);
    fixture.clock.advance(Duration::days(1));
    assert!(fixture.poll.execute().await.is_err());
    let stored = fixture.state().await;
    assert_eq!(stored.batch.len(), 1, "only `e` is left");
    let summary = fixture.revisions();

    // The feed moved on meanwhile: the replay must not look at it.
    *saves.lock().unwrap() = None;
    fixture.relay.serve(&[("g", 7), ("f", 6), ("e", 5)]);
    let fetches = fixture.relay.fetches();
    let report = fixture.pass().await;

    assert!(fixture.notification("e").await.is_some());
    assert_eq!(report.published, 1 + 2, "e replayed, then f and g");
    assert_eq!(fixture.relay.fetches(), fetches + 1);
    // `e` was upserted twice (the item in flight), nothing else was.
    assert_eq!(fixture.revisions(), summary + 3);
}

fn upsert_of(fixture: &Fixture) -> UpsertNotification {
    let store = &fixture.store;
    let fanout = Fanout::new(
        Arc::new(store.clone()),
        Arc::new(store.clone()),
        Arc::new(store.clone()),
        Arc::new(store.clone()),
        Arc::new(store.clone()),
        Arc::new(fixture.clock.clone()),
        4_096,
    );
    UpsertNotification::new(
        Arc::new(store.clone()),
        Arc::new(store.clone()),
        Arc::new(store.clone()),
        Arc::new(TestRateLimiter::new()),
        Arc::new(fixture.clock.clone()),
        Authorizer::new(Arc::new(store.clone())),
        fanout,
        Attribution::new(Arc::new(store.clone()), Arc::new(store.clone())),
    )
}

#[tokio::test]
async fn an_item_the_domain_refuses_does_not_block_the_ones_behind_it() {
    let fixture = simple(10);
    fixture.relay.serve(&[("a", 1)]);
    fixture.pass().await;
    fixture.relay.serve(&[("c", 3), ("b", 2), ("a", 1)]);
    // A stored item the domain refuses: its external id carries a control character.
    let mut state = fixture.state().await;
    state.batch = vec![notify_ports::PendingItem {
        key: Some("bad".to_owned()),
        external_id: "feed:bad\u{7}".to_owned(),
        title: "x".to_owned(),
        link: None,
    }];
    assert!(fixture.store.save(&state).await.unwrap());

    let report = fixture.pass().await;

    assert_eq!(report.published, 2, "b and c, after the refused one");
    let seen = fixture.state().await.seen.unwrap();
    assert!(seen.contains(&"bad".to_owned()), "marked seen");
}

#[tokio::test]
async fn a_follower_does_nothing() {
    let fixture = simple(10);
    let follower = PollFeeds::new(
        Arc::new(Registry(fixture.feed.clone())),
        Arc::new(fixture.store.clone()),
        Arc::new(fixture.relay.clone()),
        upsert_of(&fixture),
        Arc::new(TestLeaderElection::follower()),
        Arc::new(fixture.clock.clone()),
        Arc::new(NoMetrics),
    );
    fixture.relay.serve(&[("a", 1)]);

    let report = follower.execute().await.unwrap();

    assert!(!report.led);
    assert_eq!(fixture.relay.fetches(), 0);
}
