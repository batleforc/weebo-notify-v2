//! `EventBus` over LISTEN/NOTIFY (ADR-0008, design §4, §11): carries pokes (id + cursor), never payloads.
//! Best-effort by design: a dropped poke costs latency only, since clients catch up from their cursor.

use async_trait::async_trait;
use futures::StreamExt as _;
use futures::stream::BoxStream;
use notify_ports::{EventBus, Poke, PortError, PortResult};
use sqlx::PgPool;
use sqlx::postgres::{PgListener, PgPoolOptions};

/// The `NOTIFY` channel every replica listens on; one for all, filtering happens at the connections.
const CHANNEL: &str = "weebo_notify_pokes";

/// How long the listener waits before asking again after a failure.
const RETRY: std::time::Duration = std::time::Duration::from_secs(1);

/// What Postgres accepts as a `NOTIFY` payload, exclusive.
const NOTIFY_LIMIT: usize = 8_000;

/// Fan-out between replicas, over the database they already share.
#[derive(Debug, Clone)]
pub struct PgEventBus {
    pool: PgPool,
}

impl PgEventBus {
    /// Wrap a pool.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl EventBus for PgEventBus {
    async fn publish(&self, poke: &Poke) -> PortResult<()> {
        let mut payload = serde_json::to_string(poke)
            .map_err(|error| PortError::corrupt("eventbus.poke", error))?;
        // Unreachable with today's pokes (an id and a cursor), and the test below
        // pins that. If a future poke outgrows `NOTIFY`, a `Resync` still wakes
        // every stream and the cursor makes it lossless — better than a publish
        // that starts failing on one long channel id.
        if payload.len() >= NOTIFY_LIMIT {
            tracing::warn!(
                bytes = payload.len(),
                "poke too large for NOTIFY; sending a resync"
            );
            payload = serde_json::to_string(&Poke::Resync)
                .map_err(|error| PortError::corrupt("eventbus.poke", error))?;
        }

        // `pg_notify` binds channel and payload, so nothing is concatenated into SQL. Deliberately
        // outside the caller's transaction: an early poke is harmless since receivers read the log.
        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(CHANNEL)
            .bind(&payload)
            .execute(&self.pool)
            .await
            .map_err(|error| PortError::unavailable("eventbus", error))?;

        Ok(())
    }

    async fn subscribe(&self) -> PortResult<BoxStream<'static, Poke>> {
        // A one-connection pool of its own, from the same options: the listener
        // holds its connection for the life of the process, and drawing it from the
        // shared pool would quietly make that pool one smaller than configured
        // (high-availability.mdx counts it as "one more", outside the pool). Its
        // reconnects go through this pool too, so they never queue behind requests.
        let own = PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy_with((*self.pool.connect_options()).clone());
        let mut listener = PgListener::connect_with(&own)
            .await
            .map_err(|error| PortError::unavailable("eventbus", error))?;

        listener
            .listen(CHANNEL)
            .await
            .map_err(|error| PortError::unavailable("eventbus", error))?;

        // `try_recv`, not `into_stream`: that one reconnects silently (losing NOTIFYs) and ends on the
        // first error. Here a lost connection yields `Resync`, and an error is a pause (`lost` → `Resync`).
        let stream =
            futures::stream::unfold((listener, false), |(mut listener, mut lost)| async move {
                loop {
                    match listener.try_recv().await {
                        Ok(Some(_)) if lost => return Some((Poke::Resync, (listener, false))),
                        Ok(Some(notification)) => {
                            match serde_json::from_str::<Poke>(notification.payload()) {
                                Ok(poke) => return Some((poke, (listener, false))),
                                // Unreadable payload: likely a rolling deploy, not a reason to drop the subscription.
                                Err(error) => tracing::warn!(%error, "dropped an unreadable poke"),
                            }
                        }
                        Ok(None) => {
                            tracing::warn!(
                                "the poke listener lost its connection; resyncing streams"
                            );
                            return Some((Poke::Resync, (listener, false)));
                        }
                        Err(error) => {
                            tracing::warn!(%error, "the poke listener failed; retrying");
                            lost = true;
                            tokio::time::sleep(RETRY).await;
                        }
                    }
                }
            });

        Ok(stream.boxed())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing assertion is a failing test")]

    use notify_domain::{ChannelId, Cursor, UserId};

    use super::*;

    #[test]
    fn a_poke_fits_in_a_notify_payload() {
        // `NOTIFY` refuses more than 8000 bytes.
        let poke = Poke::Channel {
            id: ChannelId::parse("cluster-ovh-1").unwrap(),
            cursor: Cursor::from_sequence(u64::MAX),
        };

        let payload = serde_json::to_string(&poke).unwrap();

        assert!(payload.len() < 8_000, "payload was {} bytes", payload.len());
        assert_eq!(serde_json::from_str::<Poke>(&payload).unwrap(), poke);
    }

    #[test]
    fn a_private_poke_round_trips_without_naming_a_channel() {
        // A preference event is private to its user, so its poke must not name a channel.
        let poke = Poke::User {
            id: UserId::generate(),
            cursor: Cursor::from_sequence(42),
        };

        let payload = serde_json::to_string(&poke).unwrap();

        assert!(!payload.contains("channel"));
        assert_eq!(serde_json::from_str::<Poke>(&payload).unwrap(), poke);
    }
}
