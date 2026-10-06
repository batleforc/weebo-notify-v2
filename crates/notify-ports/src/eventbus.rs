//! Pokes between replicas (ADR-0008, §11): an id and a cursor, never a payload.
//! Best-effort by design: a dropped poke costs latency, the cursor catches up.

use futures::stream::BoxStream;
use notify_domain::{ChannelId, Cursor, UserId};
use serde::{Deserialize, Serialize};

use crate::error::PortResult;

/// "Something landed — go look" at the event log at `cursor`.
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
    /// A user's private preference state changed (§8.1).
    User {
        /// Which user.
        id: UserId,
        /// Where the event landed in the log.
        cursor: Cursor,
    },
    /// This replica may have missed pokes; every stream re-reads from its cursor.
    /// Raised locally by an adapter, never published.
    Resync,
}

impl Poke {
    /// Where the event landed, when the poke is about one.
    #[must_use]
    pub const fn cursor(&self) -> Option<Cursor> {
        match self {
            Self::Channel { cursor, .. } | Self::User { cursor, .. } => Some(*cursor),
            Self::Resync => None,
        }
    }
}

/// Fan-out between replicas.
#[async_trait::async_trait]
pub trait EventBus: Send + Sync {
    /// Broadcast a poke to every replica, this one included (one code path for 1 or N).
    async fn publish(&self, poke: &Poke) -> PortResult<()>;

    /// Subscribe to this instance's pokes; must survive a backend reconnect.
    async fn subscribe(&self) -> PortResult<BoxStream<'static, Poke>>;
}
