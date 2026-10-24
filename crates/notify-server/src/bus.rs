//! One `LISTEN` per replica, rebroadcast in process to every stream (best-effort, ADR-0008).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::stream::BoxStream;
use notify_ports::{EventBus, Poke, PortResult};
use tokio::sync::broadcast;

/// How many pokes a subscriber may fall behind before losing them; pokes are cheap, so generous.
const BACKLOG: usize = 1_024;

/// The longest the pump waits between two attempts to resubscribe.
const MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(30);

/// A subscription that lived this long counts as healthy, and the backoff starts
/// over after it ends.
const STABLE: std::time::Duration = std::time::Duration::from_secs(60);

/// Whether the pump is subscribed right now — what `/readyz` reports as
/// `eventbus`.
///
/// A replica whose pump is down still answers reads, so nothing else would look
/// broken while every stream it holds went stale.
#[derive(Debug, Clone)]
pub(crate) struct BusHealth(Arc<AtomicBool>);

impl BusHealth {
    pub(crate) fn is_alive(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    fn set(&self, alive: bool) {
        self.0.store(alive, Ordering::Relaxed);
    }

    /// One that reports alive, for tests that wire `/readyz` without a pump.
    #[cfg(test)]
    pub(crate) fn alive() -> Self {
        Self(Arc::new(AtomicBool::new(true)))
    }
}

/// The real bus, subscribed to once, rebroadcast in process.
#[derive(Clone)]
pub(crate) struct SharedBus {
    inner: Arc<dyn EventBus>,
    pokes: broadcast::Sender<Poke>,
    health: BusHealth,
}

impl core::fmt::Debug for SharedBus {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SharedBus")
            .field("subscribers", &self.pokes.receiver_count())
            .finish_non_exhaustive()
    }
}

impl SharedBus {
    /// Subscribe to `inner` once and start a supervised rebroadcast for the life of the process:
    /// if the subscription ends or the pump panics, it resubscribes with backoff, sends
    /// [`Poke::Resync`] and reports down on [`BusHealth`] meanwhile.
    /// Keep the returned handle and abort it before the runtime ends: dropping the `PgListener`
    /// after the runtime is gone aborts the process.
    ///
    /// # Errors
    ///
    /// Fails if the first subscription cannot be established.
    pub(crate) async fn start(
        inner: Arc<dyn EventBus>,
    ) -> PortResult<(Self, actix_web::rt::task::JoinHandle<()>)> {
        let stream = inner.subscribe().await?;
        let (pokes, _) = broadcast::channel(BACKLOG);
        let health = BusHealth(Arc::new(AtomicBool::new(true)));

        let pump = actix_web::rt::spawn(pump(
            Arc::clone(&inner),
            pokes.clone(),
            stream,
            health.clone(),
        ));

        Ok((
            Self {
                inner,
                pokes,
                health,
            },
            pump,
        ))
    }

    /// Whether the pump is subscribed, for `/readyz`.
    pub(crate) fn health(&self) -> BusHealth {
        self.health.clone()
    }
}

/// Forward pokes for ever, resubscribing whenever the subscription ends or the
/// forwarding panics.
///
/// The panic is caught in place (`catch_unwind`) rather than by spawning a child
/// task and watching its handle: the composition root aborts *this* task at
/// shutdown, and a child would outlive it — taking the `LISTEN` connection past the
/// end of the runtime, which is the abort-instead-of-exit this module's handle
/// exists to prevent.
async fn pump(
    inner: Arc<dyn EventBus>,
    sender: broadcast::Sender<Poke>,
    first: BoxStream<'static, Poke>,
    health: BusHealth,
) {
    use futures::FutureExt as _;

    let mut subscription = Some(first);
    let mut backoff = std::time::Duration::from_millis(250);

    loop {
        if let Some(stream) = subscription.take() {
            health.set(true);
            let started = std::time::Instant::now();
            let ended = std::panic::AssertUnwindSafe(forward(stream, &sender))
                .catch_unwind()
                .await;
            health.set(false);

            if ended.is_ok() {
                tracing::error!("the poke subscription ended; resubscribing");
            } else {
                tracing::error!("the poke pump panicked; resubscribing");
            }
            #[expect(
                clippy::let_underscore_must_use,
                let_underscore_drop,
                reason = "fails only with no stream open; every stream re-reads from its own cursor, which is authoritative"
            )]
            let _ = sender.send(Poke::Resync);

            if started.elapsed() > STABLE {
                backoff = std::time::Duration::from_millis(250);
            }
        }

        actix_web::rt::time::sleep(backoff).await;
        backoff = backoff.saturating_mul(2).min(MAX_BACKOFF);

        match inner.subscribe().await {
            Ok(stream) => subscription = Some(stream),
            Err(error) => tracing::warn!(?error, "could not resubscribe to the poke bus"),
        }
    }
}

/// Rebroadcast one subscription until it ends.
async fn forward(mut stream: BoxStream<'static, Poke>, sender: &broadcast::Sender<Poke>) {
    use futures::StreamExt as _;

    while let Some(poke) = stream.next().await {
        #[expect(
            clippy::let_underscore_must_use,
            let_underscore_drop,
            reason = "`send` fails only when nobody is listening, the normal state of a replica with no open streams"
        )]
        let _ = sender.send(poke);
    }
}

#[async_trait::async_trait]
impl EventBus for SharedBus {
    async fn publish(&self, poke: &Poke) -> PortResult<()> {
        // Always via the real bus, so single-replica deployments take the multi-replica path.
        self.inner.publish(poke).await
    }

    async fn subscribe(&self) -> PortResult<BoxStream<'static, Poke>> {
        let receiver = self.pokes.subscribe();

        Ok(Box::pin(async_stream(receiver)))
    }
}

/// How often a replica looks at the event watermark (ADR-0028).
///
/// Shorter than the time a read waits for it (`notify-persistence-pg`'s five
/// seconds) by enough that any row a read gave up on is seen held back on two
/// ticks in a row — which is what arms the re-poke below.
const WATERMARK_TICK: std::time::Duration = std::time::Duration::from_secs(1);

/// A hold this long is logged, once, rather than only graphed.
const WATERMARK_WARN_AFTER_SECONDS: u64 = 30;

/// Watch the event watermark for ever: export its lag, and wake every stream once
/// a long hold ends (ADR-0028).
///
/// A row committed behind a transaction still running is held back, and a read
/// waits only so long for it before serving what is safe. Nothing else would ever
/// announce the row afterwards: its own poke was spent on the read that gave up,
/// and the transaction that was holding it back may have written no event and so
/// poke nobody. A `Resync` once the watermark passes it is that announcement.
///
/// Armed only by a hold that outlived a whole tick — the newest row of the last
/// tick still past the watermark on this one — so the ordinary millisecond overlap
/// of concurrent writes never wakes every stream on the replica. Every replica
/// runs this and each `Resync` goes through the bus, so a release costs every
/// stream one read per replica; releases are rare, and that is cheaper than a
/// leader election for a watcher.
pub(crate) async fn watch_watermark(
    store: &notify_persistence_pg::PgStore,
    bus: &dyn EventBus,
    metrics: &dyn notify_ports::Metrics,
) {
    let mut tick = tokio::time::interval(WATERMARK_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut previous_newest = None;
    let mut armed = None;
    let mut warned = false;

    loop {
        tick.tick().await;
        let reading = match store.event_watermark().await {
            Ok(reading) => reading,
            Err(error) => {
                tracing::warn!(?error, "could not read the event watermark");
                continue;
            }
        };

        let lag = reading.oldest_held_back.map_or(0, |at| {
            u64::try_from((time::OffsetDateTime::now_utc() - at).whole_seconds()).unwrap_or(0)
        });
        metrics.event_watermark_lag(lag);
        if lag >= WATERMARK_WARN_AFTER_SECONDS && !warned {
            tracing::warn!(
                lag_seconds = lag,
                "a long-running transaction is holding event delivery back"
            );
            warned = true;
        } else if lag == 0 && warned {
            tracing::info!("event delivery is no longer held back");
            warned = false;
        }

        if armed.is_some_and(|target| reading.watermark >= target) {
            if let Err(error) = bus.publish(&Poke::Resync).await {
                tracing::warn!(?error, "could not announce released events");
            }
            armed = None;
        }
        if armed.is_none()
            && let Some(previous) = previous_newest
            && reading.watermark < previous
        {
            armed = Some(previous);
        }
        previous_newest = Some(reading.newest);
    }
}

/// Turn a broadcast receiver into the stream the port promises.
/// `unfold`, not `poll_fn`: `recv()` unregisters its waker when dropped, so a per-poll `recv()`
/// would never be woken and live events would silently stop.
fn async_stream(receiver: broadcast::Receiver<Poke>) -> impl futures::Stream<Item = Poke> {
    futures::stream::unfold(receiver, |mut receiver| async move {
        loop {
            match receiver.recv().await {
                Ok(poke) => return Some((poke, receiver)),
                // Lagged: lose those pokes; the cursor recovers them.
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::warn!(missed, "a stream fell behind; it will catch up by cursor");
                }
                // Closed: the pump is gone, so we are shutting down.
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::arithmetic_side_effects,
        reason = "test arithmetic on small, known values"
    )]

    use notify_domain::{ChannelId, Cursor};
    use notify_ports::test_utils::InMemory;

    use super::*;

    fn poke(sequence: u64) -> Poke {
        Poke::Channel {
            id: ChannelId::parse("ci-infra").unwrap(),
            cursor: Cursor::from_sequence(sequence),
        }
    }

    #[actix_web::test]
    async fn every_stream_gets_the_poke_from_one_subscription() {
        use futures::StreamExt as _;

        let store = InMemory::new();
        let (bus, _pump) = SharedBus::start(Arc::new(store.clone())).await.unwrap();

        // Two streams, as two browser tabs would be.
        let mut first = bus.subscribe().await.unwrap();
        let mut second = bus.subscribe().await.unwrap();

        bus.publish(&poke(1)).await.unwrap();

        // Short, so the test cannot pass on a timer wake-up instead of the poke.
        let deadline = std::time::Duration::from_millis(200);
        assert_eq!(
            actix_web::rt::time::timeout(deadline, first.next())
                .await
                .unwrap(),
            Some(poke(1))
        );
        assert_eq!(
            actix_web::rt::time::timeout(deadline, second.next())
                .await
                .unwrap(),
            Some(poke(1)),
            "one LISTEN, every connection — not one connection per client"
        );
    }

    /// A bus whose first subscription ends, whose second panics, and whose third
    /// stays up — the three things the pump has to survive.
    struct Flaky {
        subscriptions: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl EventBus for Flaky {
        async fn publish(&self, _poke: &Poke) -> PortResult<()> {
            Ok(())
        }

        async fn subscribe(&self) -> PortResult<BoxStream<'static, Poke>> {
            use futures::StreamExt as _;

            Ok(match self.subscriptions.fetch_add(1, Ordering::SeqCst) {
                0 => futures::stream::iter([poke(1)]).boxed(),
                1 => futures::stream::poll_fn(|_| panic!("the listener blew up")).boxed(),
                _ => futures::stream::pending().boxed(),
            })
        }
    }

    #[actix_web::test]
    async fn the_pump_resubscribes_after_its_subscription_ends_or_panics() {
        let flaky = Arc::new(Flaky {
            subscriptions: std::sync::atomic::AtomicUsize::new(0),
        });
        let (bus, pump) = SharedBus::start(flaky.clone()).await.unwrap();

        // 250 ms after the end, 500 ms after the panic: well inside two seconds.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while flaky.subscriptions.load(Ordering::SeqCst) < 3 {
            assert!(
                std::time::Instant::now() < deadline,
                "the pump stopped resubscribing"
            );
            actix_web::rt::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        actix_web::rt::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(
            bus.health().is_alive(),
            "subscribed again, so /readyz must say so"
        );
        assert!(!pump.is_finished(), "a panic must not end the pump");
        pump.abort();
    }

    #[actix_web::test]
    async fn a_replica_with_no_open_streams_still_publishes() {
        let store = InMemory::new();
        let (bus, _pump) = SharedBus::start(Arc::new(store.clone())).await.unwrap();

        // Nobody subscribed: publishing must still succeed.
        bus.publish(&poke(1)).await.unwrap();

        assert_eq!(store.pokes().len(), 1);
    }

    /// One event appended inside `transaction`, as the repositories do.
    async fn append(transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>, channel: &str) {
        sqlx::query(
            "INSERT INTO events (audience_kind, channel_id, recorded_at, kind, payload)
             VALUES ('channel', $1, now(), 'closure', '{}'::jsonb)",
        )
        .bind(channel)
        .execute(&mut **transaction)
        .await
        .unwrap();
    }

    /// A row held back past a whole tick, then released, gets the `Resync` nothing
    /// else would send. Needs Postgres; runs alone (`.config/nextest.toml`).
    #[actix_web::test]
    async fn watch_watermark_resyncs_streams_once_a_long_hold_ends() {
        use futures::StreamExt as _;
        use notify_ports::ChannelRepository as _;

        let Ok(url) = std::env::var("WNK_TEST_DATABASE_URL") else {
            assert!(
                std::env::var_os("WNK_TEST_REQUIRE_DB").is_none(),
                "WNK_TEST_REQUIRE_DB is set but WNK_TEST_DATABASE_URL is not — refusing to skip"
            );
            return;
        };
        let store = notify_persistence_pg::PgStore::connect(&url, 4)
            .await
            .unwrap();
        store.migrate().await.unwrap();
        let channel = ChannelId::parse(&format!(
            "t-{}",
            notify_domain::NotificationId::generate()
                .to_string()
                .trim_start_matches("ntf_")
                .to_lowercase()
        ))
        .unwrap();
        store
            .upsert(&notify_domain::Channel::dynamic(channel.clone(), None).unwrap())
            .await
            .unwrap();
        let bus = InMemory::new();
        let mut pokes = bus.subscribe().await.unwrap();
        let mut holding = store.pool().begin().await.unwrap();
        append(&mut holding, channel.as_str()).await;
        let mut behind = store.pool().begin().await.unwrap();
        append(&mut behind, channel.as_str()).await;
        behind.commit().await.unwrap();

        let watcher = {
            let (store, bus) = (store.clone(), bus.clone());
            actix_web::rt::spawn(async move {
                watch_watermark(&store, &bus, &notify_ports::NoMetrics).await;
            })
        };
        // Two ticks: the hold has to outlive one to arm the watcher.
        actix_web::rt::time::sleep(WATERMARK_TICK * 3).await;
        holding.rollback().await.unwrap();

        let resynced = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while let Some(poke) = pokes.next().await {
                if poke == Poke::Resync {
                    return;
                }
            }
        })
        .await;
        watcher.abort();
        assert!(resynced.is_ok(), "the release was never announced");
    }
}
