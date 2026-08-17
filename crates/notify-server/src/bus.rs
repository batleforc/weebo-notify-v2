//! One `LISTEN` per replica, fanned out to every connection it holds.
//!
//! # The bug this exists to fix
//!
//! [`EventBus::subscribe`] is called once per live stream — every SSE and
//! WebSocket connection asks for one. Handing each of them a real subscription
//! means one Postgres connection per connected client, and design §8.2 allows
//! twenty streams per user. A hundred people with a browser and a phone would
//! exhaust a pool sized for the whole instance, and the failure would look like
//! "the database is refusing connections" rather than "the stream transport is
//! holding them".
//!
//! Design §11 already says the poke is per *replica*: every replica learns about
//! every publication and fans it out to whichever of its connections should see
//! it. This is the "fans it out" part, and it belongs in the composition root
//! because it is a deployment concern rather than a use case.
//!
//! # What a subscriber gets
//!
//! A [`tokio::sync::broadcast`] receiver, and therefore the same best-effort
//! contract the bus itself has (ADR-0008): a slow reader that falls behind the
//! buffer loses pokes rather than blocking the others. It costs that reader
//! latency and nothing else — its next reconnect catches up from its cursor, which
//! is authoritative.

use std::sync::Arc;

use futures::stream::BoxStream;
use notify_ports::{EventBus, Poke, PortResult};
use tokio::sync::broadcast;

/// How many pokes a subscriber may fall behind before it starts losing them.
///
/// Generous, because the cost of a poke is a few dozen bytes and the cost of
/// losing one is a client that reconnects a moment later than it might have.
const BACKLOG: usize = 1_024;

/// The real bus, subscribed to once, rebroadcast in process.
#[derive(Clone)]
pub(crate) struct SharedBus {
    inner: Arc<dyn EventBus>,
    pokes: broadcast::Sender<Poke>,
}

impl core::fmt::Debug for SharedBus {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SharedBus")
            .field("subscribers", &self.pokes.receiver_count())
            .finish_non_exhaustive()
    }
}

impl SharedBus {
    /// Subscribe to `inner` once and start rebroadcasting.
    ///
    /// The pump runs for the life of the process. It reconnects by itself, because
    /// the underlying subscription does (see `notify-eventbus-pg`) — and a replica
    /// that silently stopped receiving pokes would still serve reads, so nothing
    /// would look broken while its streams went stale.
    ///
    /// The handle comes back with it, and the composition root has to keep it.
    /// The pump owns the process's one `PgListener`, and a sqlx connection dropped
    /// after the Tokio runtime has gone panics in its destructor — which aborts
    /// rather than unwinds. Aborting the pump *before* the runtime ends is what
    /// makes `SIGTERM` a clean exit instead of a core dump.
    ///
    /// # Errors
    ///
    /// Fails if the first subscription cannot be established, which is a
    /// dependency being down at boot.
    pub(crate) async fn start(
        inner: Arc<dyn EventBus>,
    ) -> PortResult<(Self, actix_web::rt::task::JoinHandle<()>)> {
        let mut stream = inner.subscribe().await?;
        let (pokes, _) = broadcast::channel(BACKLOG);

        let sender = pokes.clone();
        let pump = actix_web::rt::spawn(async move {
            use futures::StreamExt as _;

            while let Some(poke) = stream.next().await {
                // `send` fails only when nobody is listening, which is the normal
                // state of a replica with no open streams.
                let _ = sender.send(poke);
            }
            tracing::warn!("the poke subscription ended; this replica will not fan out");
        });

        Ok((Self { inner, pokes }, pump))
    }
}

#[async_trait::async_trait]
impl EventBus for SharedBus {
    async fn publish(&self, poke: &Poke) -> PortResult<()> {
        // Publishing still goes to the real bus: this replica has to tell the
        // others, and the loopback comes back through the one subscription like
        // everybody else's does. Short-circuiting it locally would make a
        // single-replica deployment take a different code path from a real one —
        // the case that only breaks in production.
        self.inner.publish(poke).await
    }

    async fn subscribe(&self) -> PortResult<BoxStream<'static, Poke>> {
        let receiver = self.pokes.subscribe();

        Ok(Box::pin(async_stream(receiver)))
    }
}

/// Turn a broadcast receiver into the stream the port promises.
///
/// `unfold` rather than `poll_fn`, and that is the whole point of this function
/// rather than a detail of it. `broadcast::Receiver::recv()` registers its waker
/// on the first poll and **unregisters it when the future is dropped** — so a
/// `poll_fn` that builds a fresh `recv()` on every poll, polls it once and drops
/// it leaves nothing behind to be woken. The next poke wakes nobody.
///
/// The failure that causes is the quiet kind: the connection stays open, the
/// keep-alive keeps flowing on its own timer, the backlog replays correctly on
/// connect, and only *live* events never arrive. `unfold` holds one `recv()`
/// future across polls, which is what keeps the registration alive.
fn async_stream(receiver: broadcast::Receiver<Poke>) -> impl futures::Stream<Item = Poke> {
    futures::stream::unfold(receiver, |mut receiver| async move {
        loop {
            match receiver.recv().await {
                Ok(poke) => return Some((poke, receiver)),
                // Lagged: this reader fell behind the buffer. It loses those pokes
                // and carries on, which is the bus's contract — the cursor
                // recovers them.
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::warn!(missed, "a stream fell behind; it will catch up by cursor");
                }
                // Closed: the pump is gone, so the process is shutting down.
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing assertion is a failing test")]

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

        // Short on purpose. A generous deadline lets this pass on the timer's
        // wake-up rather than on the poke's — which is exactly how a stream that
        // never woke on a poke looked green here while delivering nothing live.
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

    #[actix_web::test]
    async fn a_replica_with_no_open_streams_still_publishes() {
        let store = InMemory::new();
        let (bus, _pump) = SharedBus::start(Arc::new(store.clone())).await.unwrap();

        // Nobody is subscribed. `broadcast::send` fails in that state, and treating
        // that as an error would fail every publish on an idle replica.
        bus.publish(&poke(1)).await.unwrap();

        assert_eq!(store.pokes().len(), 1);
    }
}
