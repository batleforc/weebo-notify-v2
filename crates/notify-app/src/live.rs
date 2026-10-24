//! The live feed: one read of the event log per replica per wake, pushed to every open
//! stream, which filters it in memory. Without it each poke had every stream read the
//! log for itself — a thousand reads per publication for a thousand streams (TODO K1).
//! A stream that joined late or fell behind still reads for itself, through
//! [`crate::SyncBacklog`]: the feed is the hot path, not the only one.

use std::sync::Arc;

use futures::stream::BoxStream;
use futures::{FutureExt as _, StreamExt as _};
use notify_domain::{Cursor, UserId};
use notify_ports::{Event, EventStore, PageRequest, Poke, PortResult};
use tokio::sync::{broadcast, watch};

/// How many messages a stream may fall behind before it reads for itself.
const BACKLOG: usize = 256;

/// Events per read; a burst bigger than this goes out as several batches.
const PAGE: u32 = 256;

/// The most queued pokes one wake swallows, as a stream did before.
const DRAIN: usize = 1_024;

/// How long the feed waits before retrying a read that failed.
const RETRY: core::time::Duration = core::time::Duration::from_secs(1);

/// What the feed tells the streams.
#[derive(Debug, Clone)]
pub enum Live {
    /// Every event in `(after, until]`, compacted, oldest first.
    Batch(Arc<LiveBatch>),
    /// Something about this user changed (a membership, a preference): its streams
    /// re-read what they may see.
    User(UserId),
}

/// One read of the log.
#[derive(Debug)]
pub struct LiveBatch {
    /// Where the read started; a stream behind it has a gap to read for itself.
    pub after: Cursor,
    /// The last event, where the next batch starts.
    pub until: Cursor,
    /// The events, for every audience: each stream keeps its own.
    pub events: Vec<Event>,
}

/// The replica's single reader. Clone it into the HTTP state; spawn [`Self::run`] once.
#[derive(Clone)]
pub struct LiveFeed {
    events: Arc<dyn EventStore>,
    sender: broadcast::Sender<Live>,
    position: Arc<watch::Sender<Cursor>>,
}

impl core::fmt::Debug for LiveFeed {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LiveFeed")
            .field("streams", &self.sender.receiver_count())
            .finish_non_exhaustive()
    }
}

impl LiveFeed {
    #[must_use]
    pub fn new(events: Arc<dyn EventStore>) -> Self {
        Self {
            events,
            sender: broadcast::channel(BACKLOG).0,
            position: Arc::new(watch::Sender::new(Cursor::from_sequence(0))),
        }
    }

    /// Listen from now on. Subscribe before reading the backlog, so nothing published
    /// in between is missed.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Live> {
        self.sender.subscribe()
    }

    /// Everything up to here has gone out in a batch. A stream that has read the log
    /// for itself up to the watermark has seen at least this far.
    #[must_use]
    pub fn position(&self) -> Cursor {
        *self.position.borrow()
    }

    /// Read on every poke until the bus ends. Starts at the head: a stream behind it
    /// reads its own backlog.
    pub async fn run(self, mut pokes: BoxStream<'static, Poke>) {
        let mut cursor = loop {
            match self.events.head().await {
                Ok(head) => break head,
                Err(error) => {
                    tracing::warn!(%error, "the live feed cannot find the head of the log");
                    tokio::time::sleep(RETRY).await;
                }
            }
        };
        self.position.send_replace(cursor);

        let mut owed = false;
        loop {
            if !owed {
                let Some(poke) = pokes.next().await else {
                    return;
                };
                self.forward(&poke);
                // A burst of publications costs one read, not one per publication.
                for _ in 0..DRAIN {
                    match pokes.next().now_or_never() {
                        Some(Some(queued)) => self.forward(&queued),
                        Some(None) => return,
                        None => break,
                    }
                }
            }

            owed = match self.read(&mut cursor).await {
                Ok(()) => false,
                Err(error) => {
                    // The cursor did not move: the retry reads the same events.
                    tracing::warn!(%error, "the live feed could not read the log");
                    tokio::time::sleep(RETRY).await;
                    true
                }
            };
        }
    }

    fn forward(&self, poke: &Poke) {
        if let Poke::User { id, .. } = poke {
            self.send(Live::User(*id));
        }
    }

    fn send(&self, live: Live) {
        #[expect(
            clippy::let_underscore_must_use,
            let_underscore_drop,
            reason = "`send` fails only when no stream is open, the normal state of an idle replica"
        )]
        let _ = self.sender.send(live);
    }

    /// Read from `cursor` to the watermark, page by page, and send each page.
    async fn read(&self, cursor: &mut Cursor) -> PortResult<()> {
        loop {
            let page = self
                .events
                .tail(*cursor, PageRequest::of_size(PAGE))
                .await?;
            let more = page.next.is_some();
            if let Some(until) = page.items.last().map(|event| event.cursor) {
                let after = core::mem::replace(cursor, until);
                self.send(Live::Batch(Arc::new(LiveBatch {
                    after,
                    until,
                    events: page.items,
                })));
                self.position.send_replace(until);
            }
            if !more {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::channel::mpsc;
    use notify_domain::{
        Author, Channel, ChannelId, ChannelPolicy, ContentDraft, Notification, NotificationId,
        Principal, RevisionDraft, State,
    };
    use notify_ports::NotificationRepository as _;
    use notify_ports::test_utils::InMemory;
    use time::OffsetDateTime;

    use super::*;

    fn ci() -> ChannelId {
        ChannelId::parse("ci-infra").unwrap()
    }

    async fn publish(store: &InMemory, title: &str) -> Cursor {
        let (notification, written) = Notification::create(
            NotificationId::generate(),
            ci(),
            None,
            RevisionDraft::lifecycle(State::Open, ContentDraft::titled(title)),
            Author::anonymous(Principal::User {
                id: UserId::generate(),
            }),
            OffsetDateTime::now_utc(),
            &ChannelPolicy::default(),
        )
        .unwrap();
        store
            .insert(&notification, &written.revision)
            .await
            .unwrap();
        store.head().await.unwrap()
    }

    /// A feed running on `store`, past its first read of the head.
    async fn running(store: &InMemory) -> (LiveFeed, mpsc::UnboundedSender<Poke>) {
        store.seed_channel(Channel::dynamic(ci(), None).unwrap());
        publish(store, "before the feed").await;
        let feed = LiveFeed::new(Arc::new(store.clone()));
        let (pokes, receiver) = mpsc::unbounded();
        tokio::spawn(feed.clone().run(Box::pin(receiver)));
        let head = store.head().await.unwrap();
        while feed.position() != head {
            tokio::task::yield_now().await;
        }
        (feed, pokes)
    }

    #[tokio::test]
    async fn a_burst_of_pokes_costs_one_read_and_one_batch() {
        let store = InMemory::new();
        let (feed, pokes) = running(&store).await;
        let mut live = feed.subscribe();
        let start = feed.position();

        publish(&store, "one").await;
        publish(&store, "two").await;
        let last = publish(&store, "three").await;
        for _ in 0..3 {
            pokes
                .unbounded_send(Poke::Channel {
                    id: ci(),
                    cursor: last,
                })
                .unwrap();
        }

        let Live::Batch(batch) = live.recv().await.unwrap() else {
            panic!("a channel poke sends events, not a user signal");
        };
        assert_eq!((batch.after, batch.until), (start, last));
        assert_eq!(batch.events.len(), 3);
        assert!(
            live.try_recv().is_err(),
            "the queued pokes were swallowed by the same read"
        );
        assert_eq!(feed.position(), last);
    }

    #[tokio::test]
    async fn the_feed_starts_at_the_head_and_relays_user_pokes() {
        let store = InMemory::new();
        let (feed, pokes) = running(&store).await;
        let mut live = feed.subscribe();
        let user = UserId::generate();

        pokes
            .unbounded_send(Poke::User {
                id: user,
                cursor: feed.position(),
            })
            .unwrap();

        assert!(matches!(live.recv().await.unwrap(), Live::User(id) if id == user));
        assert!(
            live.try_recv().is_err(),
            "what was published before the feed started is a stream's own backlog"
        );
    }
}
