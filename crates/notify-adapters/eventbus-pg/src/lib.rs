//! `EventBus` over LISTEN/NOTIFY — carries pokes only, never payloads
//! (ADR-0008, design §4, §11).
//!
//! Each replica holds its own stream connections, so every replica has to learn
//! about every publication. This is the default way it does, and it needs no
//! infrastructure the instance did not already have: the database is the bus.
//!
//! # Pokes, not events
//!
//! A poke is an identifier and a cursor. Partly because `NOTIFY` caps its payload
//! at 8 KB and a revision does not fit, and mostly because a bus that carried
//! payloads would be a second source of truth to keep in step with the first. A
//! replica that receives one reads the authoritative row from the event log at that
//! cursor and fans it out to whichever of its connections should see it.
//!
//! # Best-effort is the design, not a shortcut
//!
//! A dropped poke costs latency and nothing else: the client's next reconnect or
//! poll catches up from its cursor, which is authoritative. That is what makes it
//! reasonable to run the default deployment on this and to swap in Redis purely for
//! scale (design §11) without the domain noticing.

use async_trait::async_trait;
use futures::StreamExt as _;
use futures::stream::BoxStream;
use notify_ports::{EventBus, Poke, PortError, PortResult};
use sqlx::PgPool;
use sqlx::postgres::PgListener;

/// The `NOTIFY` channel every replica of an instance listens on.
///
/// One channel rather than one per weebo-notify channel: `LISTEN` is per session
/// and a replica cares about every channel some connected client is subscribed to,
/// which changes as clients come and go. Filtering happens where the connections
/// are.
const CHANNEL: &str = "weebo_notify_pokes";

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
        let payload = serde_json::to_string(poke)
            .map_err(|error| PortError::corrupt("eventbus.poke", error))?;

        // `pg_notify` rather than a `NOTIFY` statement: the channel and payload are
        // bind parameters, so nothing is ever concatenated into SQL.
        //
        // This deliberately does not join the caller's transaction. A poke sent
        // inside one would only fire on commit, which sounds tidier but couples
        // fan-out latency to transaction length — and the cursor makes a poke that
        // arrives slightly early harmless: the receiver reads the log, and if the
        // row is not there yet its next poke or reconnect finds it.
        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(CHANNEL)
            .bind(&payload)
            .execute(&self.pool)
            .await
            .map_err(|error| PortError::unavailable("eventbus", error))?;

        Ok(())
    }

    async fn subscribe(&self) -> PortResult<BoxStream<'static, Poke>> {
        let mut listener = PgListener::connect_with(&self.pool)
            .await
            .map_err(|error| PortError::unavailable("eventbus", error))?;

        listener
            .listen(CHANNEL)
            .await
            .map_err(|error| PortError::unavailable("eventbus", error))?;

        // `into_stream` reconnects and re-issues the LISTEN by itself. That matters
        // more than it looks: a replica that silently stopped receiving pokes still
        // serves reads, so nothing would look broken while its streams went stale.
        let stream = listener
            .into_stream()
            .filter_map(|notification| async move {
                match notification {
                    Ok(notification) => {
                        match serde_json::from_str::<Poke>(notification.payload()) {
                            Ok(poke) => Some(poke),
                            Err(error) => {
                                // A payload this build does not understand is a rolling
                                // deploy, not a reason to tear the subscription down.
                                tracing::warn!(%error, "dropped an unreadable poke");
                                None
                            }
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "the poke listener hiccuped; it reconnects itself");
                        None
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
        // `NOTIFY` refuses more than 8000 bytes, and a poke that did not fit would
        // fail at publish time on a busy instance rather than in a test.
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
        // A preference event belongs to its user and to nobody else, so the poke
        // that announces it must not carry a channel anyone could subscribe to.
        let poke = Poke::User {
            id: UserId::generate(),
            cursor: Cursor::from_sequence(42),
        };

        let payload = serde_json::to_string(&poke).unwrap();

        assert!(!payload.contains("channel"));
        assert_eq!(serde_json::from_str::<Poke>(&payload).unwrap(), poke);
    }
}
