//! The minimal model of a stream event that a cache needs (design §8.1).
//!
//! # Why this exists at all
//!
//! ADR-0009 says clients come from the contract, and they should. They cannot here:
//! progenitor renders `Event` — an `allOf` of two `oneOf`s, which is what gives the
//! flat wire shape §8.1 documents — as `pub enum EventVariant0 {}`, an
//! **uninhabited** type. A generated client can deserialise a page of events only
//! while it contains none.
//!
//! So this is the one hand-written wire model in the client, shared by the sync loop
//! that writes the cache and the store that reads it back — one place rather than
//! two drifting copies. It is a **stated exception, not a pattern**; the fix is D1 in
//! §14.4, and when it lands this file goes away.
//!
//! # It reads what it needs and ignores the rest
//!
//! Every field is optional and unknown ones are dropped on the floor, because an
//! older client must not stall its cursor on a newer server's event kind (§7.2). The
//! *stored* payload, however, is the whole event verbatim: what this type cannot
//! name is still on disk for a later build that can.

use serde::Deserialize;

use crate::types::{State, Status};

/// One event, flattened exactly as §8.1 puts it on the wire.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WireEvent {
    /// Where the caller is up to. The one field a sync loop cannot do without.
    pub cursor: String,
    /// Which kind of event this is: `revision`, `tombstone`, `receipt`…
    pub event: String,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub notification_id: Option<String>,
    /// When the notification was first created, server-side.
    #[serde(default)]
    pub created_at: Option<String>,
    /// When this revision landed, server-side.
    #[serde(default)]
    pub revised_at: Option<String>,
    #[serde(default)]
    pub seq: Option<u32>,
    #[serde(default)]
    pub state: Option<State>,
    #[serde(default)]
    pub status: Option<Status>,
    // Priority, author and the rest of a revision are deliberately absent: they are
    // on disk in the stored event and can be read the day something draws them,
    // which is cheaper than a field nothing uses today.
    #[serde(default)]
    pub payload: Option<WireContent>,
}

/// What a row shows. The rest of a `Content` is on disk and unread here.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WireContent {
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
}
