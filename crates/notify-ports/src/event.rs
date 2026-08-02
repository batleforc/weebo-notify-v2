//! The event log: one append-only sequence behind the stream, the backlog and
//! reconnect alike (ADR-0012, design §8.1).
//!
//! Every `revision`, `receipt`, `interaction`, `tombstone` and `preference` gets a
//! row, and that row's sequence value **is** the cursor a client sends back —
//! whether as `?cursor=` on `GET /sync` or as `Last-Event-ID` on the stream. "Zero
//! gaps, zero duplicates" is then a property of the schema rather than something
//! each transport has to re-implement correctly, and a client can reconnect to any
//! replica because the order is global rather than per-connection.
//!
//! # Audience is part of the event, not a filter applied later
//!
//! A `preference` event goes to one user's own connections and to nobody else's —
//! including other members of the same channel (scenario T26). Putting that in the
//! event itself means every transport gets it right by construction, instead of
//! each stream, sync and websocket path having to remember the exception.

use notify_domain::{
    Author, ChannelId, Content, Cursor, ExternalId, InteractionRecord, Notification,
    NotificationId, Priority, Receipt, Revision, Silence, State, Status, TombstoneReason, UserId,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::PortResult;
use crate::pagination::PageRequest;

/// Who an event is for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "audience", rename_all = "snake_case")]
pub enum Audience {
    /// Every principal with `subscribe` on the channel.
    Channel {
        /// The channel.
        id: ChannelId,
    },
    /// One user's own connections, wherever they are. Never anybody else's.
    User {
        /// The user.
        id: UserId,
    },
}

impl Audience {
    /// The channel, when this is a channel event.
    #[must_use]
    pub const fn channel(&self) -> Option<&ChannelId> {
        match self {
            Self::Channel { id } => Some(id),
            Self::User { .. } => None,
        }
    }

    /// The user, when this is a private event.
    #[must_use]
    pub const fn user(&self) -> Option<UserId> {
        match self {
            Self::User { id } => Some(*id),
            Self::Channel { .. } => None,
        }
    }
}

/// The `event:` names of the stream (design §8.1). Part of the wire contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// A revision was published. Creation is included — creating *is* pushing
    /// revision `seq = 1`.
    Revision,
    /// A delivery or read receipt was posted.
    Receipt,
    /// Somebody acknowledged, replied or picked.
    Interaction,
    /// A notification stopped existing; clients drop it locally.
    Tombstone,
    /// This user's own do-not-disturb or silence state changed.
    Preference,
}

impl EventKind {
    /// The name as it appears in the `event:` field.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Revision => "revision",
            Self::Receipt => "receipt",
            Self::Interaction => "interaction",
            Self::Tombstone => "tombstone",
            Self::Preference => "preference",
        }
    }
}

/// A revision, flattened into the shape the stream publishes (design §8.1).
///
/// Denormalised on purpose: a client that just reconnected must be able to render
/// a timeline row from one event, without a follow-up request per notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevisionEvent {
    /// The notification this revises.
    pub notification_id: NotificationId,
    /// The publisher's own identifier, when it upserted by one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_id: Option<ExternalId>,
    /// Server-assigned sequence within the notification — the arbiter of
    /// last-write-wins (design §2.2).
    pub seq: u32,
    /// Lifecycle position as of this revision.
    pub state: State,
    /// Status as of this revision.
    pub status: Status,
    /// Priority as of this revision.
    pub priority: Priority,
    /// Who published it.
    pub author: Author,
    /// When the notification was first created.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// When this revision landed.
    #[serde(with = "time::serde::rfc3339")]
    pub revised_at: OffsetDateTime,
    /// The content. Always in the clear: the stream is TLS straight to us, and the
    /// sealed box exists only inside mobile push blobs (design §6.2).
    pub payload: Content,
}

impl RevisionEvent {
    /// Build from the notification and the revision that was just recorded.
    #[must_use]
    pub fn new(notification: &Notification, revision: &Revision) -> Self {
        Self {
            notification_id: notification.id,
            external_id: notification.external_id.clone(),
            seq: revision.seq,
            state: revision.state,
            status: revision.status,
            priority: revision.priority,
            author: revision.author.clone(),
            created_at: notification.created_at,
            revised_at: revision.received_at,
            payload: revision.content.clone(),
        }
    }
}

/// This user's delivery preferences changed (design §7.4, §8.1).
///
/// Carries the whole state rather than a delta: a client that missed one of these
/// would otherwise apply the next one to the wrong baseline, and "my phone kept
/// buzzing after I turned on do-not-disturb" is not a bug anybody enjoys chasing.
/// Do-not-disturb schedules join this in M3; M1 carries the silences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreferenceEvent {
    /// Whose preferences these are.
    pub user: UserId,
    /// Every silence currently in force for them.
    pub silences: Vec<Silence>,
}

/// What actually happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EventPayload {
    /// See [`RevisionEvent`]. Boxed because it is much larger than its siblings and
    /// every `Event` would otherwise pay for it.
    Revision(Box<RevisionEvent>),
    /// A receipt was posted.
    Receipt(Receipt),
    /// An interaction was recorded.
    Interaction(InteractionRecord),
    /// A notification is gone.
    Tombstone {
        /// Which one.
        notification_id: NotificationId,
        /// Why — clients word "deleted" and "expired" differently.
        reason: TombstoneReason,
    },
    /// See [`PreferenceEvent`].
    Preference(Box<PreferenceEvent>),
}

impl EventPayload {
    /// The `event:` name this payload publishes under.
    #[must_use]
    pub const fn kind(&self) -> EventKind {
        match self {
            Self::Revision(_) => EventKind::Revision,
            Self::Receipt(_) => EventKind::Receipt,
            Self::Interaction(_) => EventKind::Interaction,
            Self::Tombstone { .. } => EventKind::Tombstone,
            Self::Preference(_) => EventKind::Preference,
        }
    }

    /// The notification this concerns, when it concerns one.
    #[must_use]
    pub const fn notification_id(&self) -> Option<NotificationId> {
        match self {
            Self::Revision(revision) => Some(revision.notification_id),
            Self::Receipt(receipt) => Some(receipt.notification_id),
            Self::Interaction(interaction) => Some(interaction.notification_id),
            Self::Tombstone {
                notification_id, ..
            } => Some(*notification_id),
            Self::Preference(_) => None,
        }
    }
}

/// One row of the event log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// This row's position — the `id:` of the stream frame, and the cursor a client
    /// sends back.
    pub cursor: Cursor,
    /// Who may see it.
    #[serde(flatten)]
    pub audience: Audience,
    /// When it was appended, by the server's clock.
    #[serde(with = "time::serde::rfc3339")]
    pub recorded_at: OffsetDateTime,
    /// What happened.
    #[serde(flatten)]
    pub payload: EventPayload,
}

impl Event {
    /// The `event:` name.
    #[must_use]
    pub const fn kind(&self) -> EventKind {
        self.payload.kind()
    }

    /// Whether this event is visible to a given user, given the channels they may
    /// subscribe to.
    ///
    /// The private case is the one worth having in one place: a `preference` event
    /// belongs to its user and to nobody else, however many channels they share
    /// (scenario T26).
    #[must_use]
    pub fn is_visible_to(&self, user: UserId, subscribed: &[ChannelId]) -> bool {
        match &self.audience {
            Audience::User { id } => *id == user,
            Audience::Channel { id } => subscribed.contains(id),
        }
    }
}

/// What a client is catching up on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncQuery {
    /// Resume after this position. `None` replays everything still retained.
    pub after: Option<Cursor>,
    /// Restrict to these channels; empty means every channel the caller may read.
    /// Mirrors `?channels=a,b` on the stream (design §8.1).
    pub channels: Vec<ChannelId>,
    /// How much to return.
    pub page: PageRequest,
}

/// The answer to a catch-up, which is more than a list of events.
///
/// Two extras carry real product meaning:
///
/// * `expired` is the "N notifications expired while you were away" counter
///   (design §7.2bis). A message that expired undelivered is deliberately *not* in
///   the backlog — but silently dropping it would leave a user wondering, so the
///   count is reported per channel instead.
/// * `compacted` records how many revisions were folded away (design §7.2). Forty
///   revisions of one deploy become one entry; the number is what lets a client say
///   "38 updates" instead of pretending nothing happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncPage {
    /// The events, oldest first.
    pub events: Vec<Event>,
    /// Pass back as `after` for the next page. `None` means fully caught up.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<Cursor>,
    /// Per channel, how many notifications expired unseen during the absence.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub expired: Vec<ExpiredWhileAway>,
    /// How many revisions the compaction folded away.
    pub compacted: u32,
}

impl SyncPage {
    /// A page with nothing to catch up on.
    #[must_use]
    pub const fn caught_up() -> Self {
        Self {
            events: Vec::new(),
            next: None,
            expired: Vec::new(),
            compacted: 0,
        }
    }
}

/// "3 notifications expired in `ci-infra` while you were away."
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpiredWhileAway {
    /// The channel.
    pub channel: ChannelId,
    /// How many.
    pub count: u32,
}

/// Reading the event log.
///
/// Appends are deliberately absent: an event is written in the same transaction as
/// the state change that caused it, so the repositories own that side. A port that
/// could append on its own would make "the event log and the data disagree"
/// reachable, which is precisely what [ADR-0012] exists to prevent.
///
/// [ADR-0012]: https://github.com/batleforc/weebo-notify-v2/blob/main/docs/content/docs/adr/0012-single-events-table-as-the-cursor.mdx
#[async_trait::async_trait]
pub trait EventStore: Send + Sync {
    /// Replay what a client missed, from its cursor (design §8.1, scenario T7).
    ///
    /// The contract the tests hold this to: exactly the events after `after`, in
    /// order, with no gap and no duplicate — and the same cursor works whether it
    /// arrived as `?cursor=` or as `Last-Event-ID`.
    async fn sync(&self, user: UserId, query: &SyncQuery) -> PortResult<SyncPage>;

    /// The newest position in the log, for a client that wants to start from
    /// "now" rather than replay history.
    async fn head(&self) -> PortResult<Cursor>;

    /// Drop events older than `before`.
    ///
    /// Retention here is its own job, separate from notification TTL: an expired
    /// notification's events have to outlive it long enough for a slow client to
    /// catch up, and then go ([ADR-0012], accepted debt). Leader-only (design §11).
    async fn prune(&self, before: OffsetDateTime, limit: u32) -> PortResult<u64>;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use notify_domain::{Principal, UserId};

    use super::*;

    fn user() -> UserId {
        UserId::generate()
    }

    fn event(audience: Audience) -> Event {
        Event {
            cursor: Cursor::from_sequence(1),
            audience,
            recorded_at: OffsetDateTime::from_unix_timestamp(1_785_000_000).unwrap(),
            payload: EventPayload::Tombstone {
                notification_id: NotificationId::generate(),
                reason: TombstoneReason::Expired,
            },
        }
    }

    // --- T26 · Private events stay private (design §3.4) ------------------

    #[test]
    fn critical_t26_a_preference_event_reaches_only_its_own_user() {
        let (alice, bob) = (user(), user());
        let channel = ChannelId::parse("ci-infra").unwrap();
        let shared = [channel.clone()];

        let private = Event {
            payload: EventPayload::Preference(Box::new(PreferenceEvent {
                user: alice,
                silences: Vec::new(),
            })),
            ..event(Audience::User { id: alice })
        };

        assert!(private.is_visible_to(alice, &shared));
        assert!(
            !private.is_visible_to(bob, &shared),
            "sharing a channel with somebody does not make their silences your business"
        );
    }

    #[test]
    fn a_channel_event_reaches_exactly_its_subscribers() {
        let alice = user();
        let mine = ChannelId::parse("ci-infra").unwrap();
        let theirs = ChannelId::parse("cluster-ovh-1").unwrap();

        let published = event(Audience::Channel { id: mine.clone() });

        assert!(published.is_visible_to(alice, &[mine]));
        assert!(!published.is_visible_to(alice, &[theirs]));
        assert!(!published.is_visible_to(alice, &[]));
    }

    #[test]
    fn every_kind_keeps_its_wire_name() {
        // These strings are the `event:` field clients switch on.
        for (kind, name) in [
            (EventKind::Revision, "revision"),
            (EventKind::Receipt, "receipt"),
            (EventKind::Interaction, "interaction"),
            (EventKind::Tombstone, "tombstone"),
            (EventKind::Preference, "preference"),
        ] {
            assert_eq!(kind.as_str(), name);
        }
    }

    #[test]
    fn a_revision_event_carries_enough_to_render_without_a_follow_up() {
        use notify_domain::{Author, ChannelPolicy, ContentDraft, RevisionDraft};

        let now = OffsetDateTime::from_unix_timestamp(1_785_000_000).unwrap();
        let author = Author::anonymous(Principal::User { id: user() });
        let (notification, written) = Notification::create(
            NotificationId::generate(),
            ChannelId::parse("ci-infra").unwrap(),
            Some(ExternalId::parse("run-4521").unwrap()),
            RevisionDraft::lifecycle(State::Open, ContentDraft::titled("deploy prod")),
            author,
            now,
            &ChannelPolicy::default(),
        )
        .unwrap();

        let rendered = RevisionEvent::new(&notification, &written.revision);

        assert_eq!(rendered.seq, 1);
        assert_eq!(rendered.state, State::Open);
        assert_eq!(rendered.payload.title, "deploy prod");
        assert_eq!(
            rendered.external_id.as_ref().map(ExternalId::as_str),
            Some("run-4521")
        );
        assert_eq!(rendered.created_at, rendered.revised_at);

        let payload = EventPayload::Revision(Box::new(rendered));
        assert_eq!(payload.kind(), EventKind::Revision);
        assert_eq!(payload.notification_id(), Some(notification.id));
    }

    #[test]
    fn a_preference_event_belongs_to_no_notification() {
        let payload = EventPayload::Preference(Box::new(PreferenceEvent {
            user: user(),
            silences: Vec::new(),
        }));
        assert_eq!(payload.notification_id(), None);
    }

    #[test]
    fn an_event_round_trips_through_json() {
        let original = event(Audience::Channel {
            id: ChannelId::parse("ci-infra").unwrap(),
        });
        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains("\"event\":\"tombstone\""));
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), original);
    }
}
