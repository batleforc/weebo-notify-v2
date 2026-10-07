//! One `LISTEN` per replica, rebroadcast in process to every stream (best-effort, ADR-0008).

use std::sync::Arc;

use futures::stream::BoxStream;
use notify_ports::{EventBus, Poke, PortResult};
use tokio::sync::broadcast;

/// How many pokes a subscriber may fall behind before losing them; pokes are cheap, so generous.
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
    /// Subscribe to `inner` once and start rebroadcasting for the life of the process.
    /// Keep the returned handle and abort it before the runtime ends: dropping the `PgListener`
    /// after the runtime is gone aborts the process.
    ///
    /// # Errors
    ///
    /// Fails if the first subscription cannot be established.
    pub(crate) async fn start(
        inner: Arc<dyn EventBus>,
    ) -> PortResult<(Self, actix_web::rt::task::JoinHandle<()>)> {
        let mut stream = inner.subscribe().await?;
        let (pokes, _) = broadcast::channel(BACKLOG);

        let sender = pokes.clone();
        let pump = actix_web::rt::spawn(async move {
            use futures::StreamExt as _;

            while let Some(poke) = stream.next().await {
                // `send` fails only when nobody is listening: normal on an idle replica.
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
        // Always via the real bus, so single-replica deployments take the multi-replica path.
        self.inner.publish(poke).await
    }

    async fn subscribe(&self) -> PortResult<BoxStream<'static, Poke>> {
        let receiver = self.pokes.subscribe();

        Ok(Box::pin(async_stream(receiver)))
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

    #[actix_web::test]
    async fn a_replica_with_no_open_streams_still_publishes() {
        let store = InMemory::new();
        let (bus, _pump) = SharedBus::start(Arc::new(store.clone())).await.unwrap();

        // Nobody subscribed: publishing must still succeed.
        bus.publish(&poke(1)).await.unwrap();

        assert_eq!(store.pokes().len(), 1);
    }
}
