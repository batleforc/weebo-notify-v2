//! Telling the other replicas something happened (ADR-0008, design §11).
//!
//! Each replica holds its own stream connections, so every replica has to learn
//! about every publication. The bus carries **pokes only** — an identifier and a
//! cursor, never a payload. Partly because Postgres `NOTIFY` caps out at 8 KB, and
//! mostly because it means the bus is not a second source of truth to keep in step
//! with the first.
//!
//! **Best-effort is sufficient, and that is a design property rather than a
//! shortcut.** A dropped poke costs latency: the client's next reconnect or poll
//! catches up from the cursor, which is authoritative. That is what makes it
//! reasonable to run the default deployment on `LISTEN/NOTIFY` with no extra
//! infrastructure, and to swap in Redis purely for scale without the domain
//! noticing.

use futures::stream::BoxStream;
use notify_domain::{ChannelId, Cursor, UserId};
use serde::{Deserialize, Serialize};

use crate::error::PortResult;

/// "Something landed — go look."
///
/// Deliberately not the event itself. A replica receiving this reads the
/// authoritative row from the event log at `cursor` and fans it out to whichever of
/// its connections should see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "poke", rename_all = "snake_case")]
pub enum Poke {
    /// A channel got a new event.
    Channel {
        /// Which channel.
        id: ChannelId,
        /// Where the event landed in the log.
        cursor: Cursor,
    },
    /// A user's own preference state changed (design §8.1) — private, so it is
    /// addressed to the user rather than to any channel.
    User {
        /// Which user.
        id: UserId,
        /// Where the event landed in the log.
        cursor: Cursor,
    },
}

impl Poke {
    /// Where the event landed.
    #[must_use]
    pub const fn cursor(&self) -> Cursor {
        match self {
            Self::Channel { cursor, .. } | Self::User { cursor, .. } => *cursor,
        }
    }
}

/// Fan-out between replicas.
#[async_trait::async_trait]
pub trait EventBus: Send + Sync {
    /// Broadcast a poke to every replica, this one included.
    ///
    /// Including itself is what keeps single-replica and multi-replica deployments
    /// on the same code path — the case that would otherwise only break in
    /// production.
    async fn publish(&self, poke: &Poke) -> PortResult<()>;

    /// Subscribe to the pokes of this instance.
    ///
    /// The stream is expected to survive a backend reconnect: a replica that
    /// silently stopped receiving pokes still serves reads, so nothing would look
    /// broken while its streams quietly went stale.
    async fn subscribe(&self) -> PortResult<BoxStream<'static, Poke>>;
}
