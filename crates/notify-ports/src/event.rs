//! The event log: one global append-only sequence whose position is the cursor
//! for stream, sync and reconnect (ADR-0012, §8.1). Audience is part of each event (T26).

use notify_domain::{
    Author, ChannelId, Content, Cursor, Dnd, ExternalId, InteractionRecord, Notification,
    NotificationId, Priority, Receipt, Revision, Silence, State, Status, TombstoneReason, UserId,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::PortResult;
use crate::pagination::PageRequest;

/// Who an event is for. Wire names are `channel_id`/`user_id` (flattened next to
/// payloads that own an `id`); its schema lives in `crate::schema`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "audience", rename_all = "snake_case")]
pub enum Audience {
    /// Every principal with `subscribe` on the channel.
    Channel {
        /// The channel.
        #[serde(rename = "channel_id")]
        id: ChannelId,
    },
    /// One user's own connections, wherever they are. Never anybody else's.
    User {
        /// The user.
        #[serde(rename = "user_id")]
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
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// A revision was published (creation is revision `seq = 1`).
    Revision,
    /// A delivery or read receipt was posted.
    Receipt,
    /// Somebody acknowledged, replied or picked.
    Interaction,
    /// A notification stopped existing; clients drop it locally.
    Tombstone,
    /// A lifecycle notification was closed by the server for inactivity; clients mark
    /// their copy closed rather than dropping it.
    Closure,
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
            Self::Closure => "closure",
            Self::Preference => "preference",
        }
    }
}

/// A revision, denormalised so a reconnecting client can render it from one event (§8.1).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevisionEvent {
    /// The notification this revises.
    pub notification_id: NotificationId,
    /// The publisher's own identifier, when it upserted by one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_id: Option<ExternalId>,
    /// Server-assigned sequence within the notification, the last-write-wins arbiter (§2.2).
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
    /// The content, in the clear (TLS stream; sealing only applies to push, §6.2).
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

/// This user's whole delivery preference state, DND and silences together (§7.4, §8.1).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreferenceEvent {
    /// Whose preferences these are.
    pub user: UserId,
    /// Their do-not-disturb, whole (design §7.4).
    pub dnd: Dnd,
    /// Every silence currently in force for them.
    pub silences: Vec<Silence>,
}

/// What actually happened (schema hand-written in `crate::schema`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EventPayload {
    /// See [`RevisionEvent`]; boxed because it is much larger than its siblings.
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
    /// A lifecycle notification was closed by the server for inactivity (§7.2bis).
    /// Publisher-requested closures arrive as revisions instead.
    Closure {
        /// Which one.
        notification_id: NotificationId,
        /// When it was closed, which starts its retention clock.
        #[serde(with = "time::serde::rfc3339")]
        closed_at: OffsetDateTime,
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
            Self::Closure { .. } => EventKind::Closure,
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
            }
            | Self::Closure {
                notification_id, ..
            } => Some(*notification_id),
            Self::Preference(_) => None,
        }
    }
}

/// One row of the event log, as one flat object (§8.1). Audience and payload are
/// flattened into the same namespace, so the envelope reserves `channel_id`/`user_id`
/// (not `id`) and `logged_at` (not `recorded_at`); a colliding payload field is a
/// wire break. Its schema is hand-written in `crate::schema`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// This row's position: the stream frame `id:` and the cursor a client sends back.
    pub cursor: Cursor,
    /// Who may see it.
    #[serde(flatten)]
    pub audience: Audience,
    /// When it was appended, by the server's clock.
    #[serde(rename = "logged_at", with = "time::serde::rfc3339")]
    pub recorded_at: OffsetDateTime,
    /// Whether the recipient's do-not-disturb was running (§7.4). Set per reader on
    /// the way out, never stored; decides whether a client makes a noise.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub muted_by_dnd: bool,
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

    /// Whether this event is visible to a user given their readable channels;
    /// `preference` events only ever reach their own user (T26).
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
    /// Restrict to these channels; empty means every channel the caller may read (§8.1).
    pub channels: Vec<ChannelId>,
    /// The groups the caller's token carries, for group-granted channels (§11).
    pub groups: Vec<String>,
    /// How much to return.
    pub page: PageRequest,
}

/// A catch-up page: events plus the per-user expired counts (§7.2bis), the
/// compaction count (§7.2) and DND-deferred ids (§7.4).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
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
    /// How many revisions the compaction folded away, over the whole window since
    /// `after` and not just this page — so the first page of a catch-up carries the
    /// total, and that is the one a client reads it from.
    ///
    /// Counted over at most the first ten thousand events of the window (in no
    /// particular order): past that it is a **lower bound**, "at least this many",
    /// rather than a count that could cost a long absence its catch-up.
    pub compacted: u32,
    /// Notifications on this page delayed on purpose by a quiet period (§7.4); kept
    /// off the events because events are identical for every reader.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deferred_by_dnd: Vec<NotificationId>,
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
            deferred_by_dnd: Vec::new(),
        }
    }
}

/// "3 notifications expired in `ci-infra` while you were away."
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpiredWhileAway {
    /// The channel.
    pub channel: ChannelId,
    /// How many.
    pub count: u32,
}

/// Reading the event log. No append: events are written in the same transaction
/// as their state change, by the repositories (ADR-0012).
#[async_trait::async_trait]
pub trait EventStore: Send + Sync {
    /// Replay what a client missed after its cursor, without gaps or duplicates (§8.1, T7).
    async fn sync(&self, user: UserId, query: &SyncQuery) -> PortResult<SyncPage>;

    /// The newest position in the log, to start from "now".
    async fn head(&self) -> PortResult<Cursor>;

    /// Drop events older than `before`, separate from notification TTL. Leader-only (§11).
    async fn prune(&self, before: OffsetDateTime, limit: u32) -> PortResult<u64>;
}

#[cfg(test)]
mod tests {
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
            muted_by_dnd: false,
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
                dnd: Dnd::off(alice),
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
            (EventKind::Closure, "closure"),
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
        let alice = user();
        let payload = EventPayload::Preference(Box::new(PreferenceEvent {
            user: alice,
            dnd: Dnd::off(alice),
            silences: Vec::new(),
        }));
        assert_eq!(payload.notification_id(), None);
    }

    /// A `receipt` payload owns an `id`, so the audience may not also be called `id`.
    #[test]
    fn a_receipt_event_keeps_the_audience_and_the_receipt_apart() {
        use notify_domain::{ReceiptId, ReceiptKind};

        let receipt = Receipt {
            id: ReceiptId::generate(),
            notification_id: NotificationId::generate(),
            principal: Principal::User { id: user() },
            device: None,
            kind: ReceiptKind::Seen,
            at: OffsetDateTime::from_unix_timestamp(1_785_000_000).unwrap(),
        };
        let receipt_id = receipt.id.to_string();
        let original = Event {
            payload: EventPayload::Receipt(receipt),
            ..event(Audience::Channel {
                id: ChannelId::parse("ci-infra").unwrap(),
            })
        };

        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains("\"channel_id\":\"ci-infra\""), "{json}");
        assert!(json.contains(&format!("\"id\":\"{receipt_id}\"")), "{json}");
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), original);
    }

    /// An `interaction` payload owns `recorded_at`, so the envelope's is `logged_at`.
    #[test]
    fn an_interaction_event_keeps_the_two_timestamps_apart() {
        use notify_domain::{Author, Interaction, InteractionId, InteractionRecord};

        let recorded_at = OffsetDateTime::from_unix_timestamp(1_785_000_042).unwrap();
        let original = Event {
            payload: EventPayload::Interaction(InteractionRecord {
                id: InteractionId::generate(),
                notification_id: NotificationId::generate(),
                author: Author::anonymous(Principal::User { id: user() }),
                interaction: Interaction::Reply {
                    text: "on it".to_owned(),
                },
                recorded_at,
            }),
            ..event(Audience::Channel {
                id: ChannelId::parse("ci-infra").unwrap(),
            })
        };

        let json = serde_json::to_string(&original).unwrap();
        assert!(
            json.contains("\"logged_at\":\"2026-07-25T17:20:00Z\""),
            "{json}"
        );
        assert!(
            json.contains("\"recorded_at\":\"2026-07-25T17:20:42Z\""),
            "{json}"
        );
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), original);
    }

    /// `time` defaults to a nine-element array for `OffsetDateTime`; must be RFC 3339.
    #[test]
    fn a_closure_event_carries_an_rfc3339_instant() {
        let payload = EventPayload::Closure {
            notification_id: NotificationId::generate(),
            closed_at: OffsetDateTime::from_unix_timestamp(1_785_000_000).unwrap(),
        };
        let json = serde_json::to_value(&payload).unwrap();

        assert_eq!(
            json["closed_at"].as_str(),
            Some("2026-07-25T17:20:00Z"),
            "a `date-time` in the schema has to be a string on the wire: {json}"
        );
        assert_eq!(
            serde_json::from_value::<EventPayload>(json).unwrap(),
            payload
        );
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
