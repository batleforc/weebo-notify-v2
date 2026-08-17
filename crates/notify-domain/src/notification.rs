//! The notification itself: a lifecycle object, not a message (design §2.2).
//!
//! ```text
//! open ⇄ working ──▶ closed(status) ──▶ reopenable
//! ```
//!
//! Four rules carry most of the weight, and each one exists because of a concrete
//! failure it prevents:
//!
//! * **Presence of `state` at creation decides the type.** With it, the
//!   notification has a lifecycle and cannot expire while it is still running — a
//!   two-day incident stays visible for two days. Without it, the notification is
//!   *simple*: born closed, ageing from creation, which is what a fire-and-forget
//!   `curl` wants.
//! * **A closed notification stays writable.** A CI re-run pushes onto the same
//!   `external_id` and reopens it; the retention clock is suspended again and
//!   restarts from zero at the next close.
//! * **Last write wins.** Concurrent publishers — retries, parallel jobs — do not
//!   need to coordinate. The server stamps a monotonic `seq` on arrival and the
//!   current state is the one carried by the highest `seq`. No `If-Match`, no lost
//!   update to arbitrate: the whole history is kept and doubles as the audit trail.
//! * **Revisions are capped.** The 1 001st is refused and the notification is left
//!   intact and readable (design §8.2) — a publisher in a loop degrades its own
//!   notification, not the channel.
//!
//! This type holds the *head* revision and the counters, not the full history.
//! Loading a thousand revisions to append the thousand-and-first would be a strange
//! thing to make the hot path do; the timeline is a query, served straight from the
//! events table (ADR-0012).

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::channel::ChannelPolicy;
use crate::content::{Content, ContentDraft};
use crate::error::{DomainError, Result};
use crate::ids::{ChannelId, ExternalId, NotificationId, RevisionId};
use crate::limits::MAX_REVISIONS;
use crate::principal::Author;
use crate::ttl::{Ttl, TtlResolution};

/// Where a notification is in its lifecycle.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Live and unattended.
    Open,
    /// Live and being worked on.
    Working,
    /// Finished. Only from here does the retention clock run.
    Closed,
}

impl State {
    /// Whether the notification is still running, and therefore cannot expire.
    #[must_use]
    pub const fn is_live(self) -> bool {
        matches!(self, Self::Open | Self::Working)
    }
}

/// What the notification is saying, orthogonally to where it is in its lifecycle.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// It worked.
    Success,
    /// It did not.
    Failure,
    /// It worked, with something to look at.
    Warning,
    /// Neither good nor bad — the default when a publisher says nothing.
    Info,
    /// It was stopped on purpose.
    Cancelled,
}

/// How loudly a notification asks for attention (design §2.2).
///
/// `Urgent` does not bypass a user's do-not-disturb unless that user opted in:
/// the recipient decides what may wake them, never the publisher (design §7.4).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    /// Silent, inbox only.
    Min,
    /// Quiet.
    Low,
    /// The default.
    Default,
    /// Worth an immediate look.
    High,
    /// Wake me up — subject to the recipient's own rules.
    Urgent,
}

/// Which of the two publishing shapes produced this notification (design §2.2).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationKind {
    /// Published with a `state`: it has a life, and ages only once it is over.
    Lifecycle,
    /// Published without a `state`: born closed, ages from creation.
    Simple,
}

/// Why a notification stopped existing, as broadcast in a `tombstone` event
/// (design §8.1).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TombstoneReason {
    /// Somebody deleted it.
    Deleted,
    /// Its retention ran out.
    Expired,
}

/// One immutable step of a notification's life.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revision {
    /// Identity of this revision.
    pub id: RevisionId,
    /// The notification it belongs to.
    pub notification_id: NotificationId,
    /// Server-assigned, monotonic within the notification, starting at 1. The
    /// arbiter of last-write-wins.
    pub seq: u32,
    /// The principal that published it (design §5).
    pub author: Author,
    /// When the server received it — the server's clock, not the publisher's.
    #[serde(with = "time::serde::rfc3339")]
    pub received_at: OffsetDateTime,
    /// Lifecycle position as of this revision.
    pub state: State,
    /// Status as of this revision.
    pub status: Status,
    /// Priority as of this revision.
    pub priority: Priority,
    /// What it says.
    pub content: Content,
}

/// A revision as a publisher submits it: everything optional except the content.
///
/// An absent field means "leave it as it is" on an update, and takes the channel
/// default on a creation. That is what lets a CI job push `state=closed,
/// status=success` without restating the title, the tags and the buttons.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Deserialize)]
pub struct RevisionDraft {
    /// Lifecycle position. Its presence at creation is what makes the notification
    /// a lifecycle one (design §2.2).
    #[serde(default)]
    pub state: Option<State>,
    /// Status.
    #[serde(default)]
    pub status: Option<Status>,
    /// Priority.
    #[serde(default)]
    pub priority: Option<Priority>,
    /// Retention, resolved against the channel policy (design §7.2bis).
    #[serde(default)]
    pub ttl: Option<Ttl>,
    /// The content itself, flattened into the same JSON object.
    #[serde(flatten)]
    pub content: ContentDraft,
}

impl RevisionDraft {
    /// A draft carrying only content — the fire-and-forget shape.
    #[must_use]
    pub fn simple(content: ContentDraft) -> Self {
        Self {
            state: None,
            status: None,
            priority: None,
            ttl: None,
            content,
        }
    }

    /// A draft that puts the notification in `state` — the lifecycle shape.
    #[must_use]
    pub fn lifecycle(state: State, content: ContentDraft) -> Self {
        Self {
            state: Some(state),
            ..Self::simple(content)
        }
    }
}

/// The result of writing to a notification: the revision that was recorded, and
/// what the channel policy did to the requested retention.
///
/// The resolution travels back to the publisher because a clamp they cannot see is
/// a clamp they will report as a bug (scenario T6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    /// The revision as recorded, with its assigned `seq`.
    pub revision: Revision,
    /// What retention ended up being applied.
    pub retention: TtlResolution,
}

/// A notification and its current state.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    /// Stable across every revision.
    pub id: NotificationId,
    /// The channel it lives in.
    pub channel: ChannelId,
    /// The publisher's own identifier, when it upserted by one (design §2.2).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_id: Option<ExternalId>,
    /// Which publishing shape created it.
    pub kind: NotificationKind,
    /// The highest-`seq` revision applied so far — the current truth.
    pub head: Revision,
    /// Highest `seq` assigned. The next revision gets `last_seq + 1`.
    pub last_seq: u32,
    /// How many revisions have been recorded. Equal to `last_seq` unless the
    /// aggregate was rehydrated from a partial history.
    pub revision_count: u32,
    /// When the first revision landed.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// When it last closed, and therefore when its retention clock started.
    /// `None` while it is live — which is exactly why a running incident cannot
    /// expire (design §7.2bis).
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub closed_at: Option<OffsetDateTime>,
    /// Retention as resolved by the channel policy.
    pub ttl: Ttl,
    /// Set on deletion; the row survives long enough to broadcast a tombstone and
    /// to keep sync cursors honest (design §7.2bis).
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub deleted_at: Option<OffsetDateTime>,
}

impl Notification {
    /// Publish the first revision.
    ///
    /// `external_id` is `Some` when this came through the idempotent upsert route
    /// (design §8). The caller supplies `now` — the domain never reads a clock, it
    /// is told the time (the `Clock` port, design §4).
    pub fn create(
        id: NotificationId,
        channel: ChannelId,
        external_id: Option<ExternalId>,
        draft: RevisionDraft,
        author: Author,
        now: OffsetDateTime,
        policy: &ChannelPolicy,
    ) -> Result<(Self, Written)> {
        // The whole type distinction, in one line: a publisher that named a state
        // is describing something with a life; one that did not is sending a message.
        let kind = if draft.state.is_some() {
            NotificationKind::Lifecycle
        } else {
            NotificationKind::Simple
        };

        let retention = policy.ttl.resolve(draft.ttl)?;
        let state = draft.state.unwrap_or(State::Closed);
        let content: Content = draft.content.try_into()?;

        let head = Revision {
            id: RevisionId::generate(),
            notification_id: id,
            seq: 1,
            author,
            received_at: now,
            state,
            status: draft.status.unwrap_or(Status::Info),
            priority: draft.priority.unwrap_or(policy.default_priority),
            content,
        };

        let notification = Self {
            id,
            channel,
            external_id,
            kind,
            // A simple notification is born closed, so its clock starts now. That
            // single assignment is what makes retention uniform: expiry is always
            // measured from `closed_at`, whatever the kind.
            closed_at: (state == State::Closed).then_some(now),
            created_at: now,
            last_seq: 1,
            revision_count: 1,
            ttl: retention.ttl,
            deleted_at: None,
            head: head.clone(),
        };

        Ok((
            notification,
            Written {
                revision: head,
                retention,
            },
        ))
    }

    /// Record another revision.
    ///
    /// Absent fields inherit from the current head, so an update can be as small as
    /// `{"state":"closed","status":"success"}`.
    pub fn revise(
        &mut self,
        draft: RevisionDraft,
        author: Author,
        now: OffsetDateTime,
        policy: &ChannelPolicy,
    ) -> Result<Written> {
        if self.deleted_at.is_some() {
            return Err(DomainError::NotificationGone);
        }
        if self.last_seq >= MAX_REVISIONS {
            return Err(DomainError::RevisionLimitReached { max: MAX_REVISIONS });
        }

        // Only re-resolve retention when the publisher asked; otherwise the value
        // already agreed with the channel stands.
        let retention = match draft.ttl {
            Some(_) => policy.ttl.resolve(draft.ttl)?,
            None => TtlResolution {
                ttl: self.ttl,
                clamped_from: None,
            },
        };
        // Absent content keeps what the head says (design §2.2), which is what makes
        // `{"state": "closed", "status": "success"}` a complete revision.
        let content: Content = draft.content.inheriting(&self.head.content).try_into()?;

        let revision = Revision {
            id: RevisionId::generate(),
            notification_id: self.id,
            seq: self.last_seq + 1,
            author,
            received_at: now,
            state: draft.state.unwrap_or(self.head.state),
            status: draft.status.unwrap_or(self.head.status),
            priority: draft.priority.unwrap_or(self.head.priority),
            content,
        };

        self.ttl = retention.ttl;
        self.apply(&revision);

        Ok(Written {
            revision,
            retention,
        })
    }

    /// Fold a revision into the current state, last-write-wins.
    ///
    /// Used both by [`Self::revise`] and when rehydrating from storage. A revision
    /// with a `seq` below the current head is still counted — it happened — but it
    /// does not change what the notification currently says (scenario T3).
    ///
    /// Replay in `seq` order for exact `closed_at` timestamps: out of order, the
    /// final state is still correct, but the moment the retention clock started is
    /// only as accurate as the order it was told about.
    pub fn apply(&mut self, revision: &Revision) {
        self.revision_count = self.revision_count.saturating_add(1);

        if revision.seq <= self.last_seq {
            return;
        }

        let was_live = self.head.state.is_live();
        self.last_seq = revision.seq;
        self.head = revision.clone();

        match revision.state {
            // Closing starts the retention clock. Already-closed stays where it
            // was: amending a status must not silently extend retention.
            State::Closed if was_live => self.closed_at = Some(revision.received_at),
            State::Closed => {}
            // Reopening suspends it again, and it restarts from zero at the next
            // close (design §2.2, scenario T2).
            State::Open | State::Working => {
                self.closed_at = None;
                // A simple notification that gets reopened has, by definition,
                // acquired a life.
                self.kind = NotificationKind::Lifecycle;
            }
        }
    }

    /// Current lifecycle position.
    #[must_use]
    pub const fn state(&self) -> State {
        self.head.state
    }

    /// Current status.
    #[must_use]
    pub const fn status(&self) -> Status {
        self.head.status
    }

    /// Current priority.
    #[must_use]
    pub const fn priority(&self) -> Priority {
        self.head.priority
    }

    /// When this notification becomes eligible for purge.
    ///
    /// `None` means "not yet, and possibly never": either it is still live, or the
    /// channel granted it unbounded retention.
    #[must_use]
    pub fn expires_at(&self) -> Option<OffsetDateTime> {
        let closed_at = self.closed_at?;
        let ttl = self.ttl.duration()?;
        closed_at.checked_add(time::Duration::seconds(i64::try_from(ttl.as_secs()).ok()?))
    }

    /// Whether retention has run out as of `now`.
    #[must_use]
    pub fn is_expired(&self, now: OffsetDateTime) -> bool {
        self.expires_at().is_some_and(|deadline| now >= deadline)
    }

    /// Whether it is gone, either way.
    #[must_use]
    pub fn is_gone(&self, now: OffsetDateTime) -> bool {
        self.deleted_at.is_some() || self.is_expired(now)
    }

    /// Delete it. Idempotent: deleting twice keeps the first timestamp, so a
    /// retried request cannot move a tombstone.
    pub fn delete(&mut self, now: OffsetDateTime) {
        self.deleted_at.get_or_insert(now);
    }

    /// Close it on the publisher's behalf, because nothing has happened to it for
    /// longer than its channel allows (design §7.2bis, [`CloseAfter`]).
    ///
    /// Returns whether this changed anything, so a sweep does not write a row and
    /// announce a closure it did not perform.
    ///
    /// # Why this is not a revision
    ///
    /// Every other state change is one, and has an author, because somebody asked
    /// for it. Nobody asked for this one: it is the absence of anybody asking, for
    /// a week. Minting a revision would mean minting a principal to sign it — a
    /// system identity that would then exist everywhere principals do, in the
    /// audit log and in every client's author rendering, to express "no author".
    ///
    /// So the history stays honest: the last revision still records that the
    /// publisher said `open`, because they did, and `EventPayload::Closure` is what
    /// says the server closed it afterwards. What changes here is the aggregate's
    /// current state, which is the thing that was never true — a notification
    /// nobody will ever touch again is not open.
    ///
    /// [`CloseAfter`]: crate::CloseAfter
    pub fn close(&mut self, at: OffsetDateTime) -> bool {
        if !self.head.state.is_live() {
            return false;
        }
        self.head.state = State::Closed;
        // Same rule as [`Self::apply`]'s: closing is what starts the retention
        // clock, and it starts now rather than at the last revision — retention is
        // measured from the moment a notification stopped being live, and that
        // moment is this one.
        self.closed_at = Some(at);
        true
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use time::Duration as TimeDuration;

    use super::*;
    use crate::duration::HumanDuration;
    use crate::ids::UserId;
    use crate::principal::Principal;
    use crate::ttl::TtlPolicy;

    fn now() -> OffsetDateTime {
        // A fixed instant: the domain is told the time, so the tests can pick one.
        OffsetDateTime::from_unix_timestamp(1_785_000_000).unwrap()
    }

    fn author() -> Author {
        Author::anonymous(Principal::User {
            id: UserId::generate(),
        })
    }

    fn channel() -> ChannelId {
        ChannelId::parse("ci-infra").unwrap()
    }

    fn policy() -> ChannelPolicy {
        ChannelPolicy::default()
    }

    fn create(draft: RevisionDraft, at: OffsetDateTime) -> Notification {
        Notification::create(
            NotificationId::generate(),
            channel(),
            None,
            draft,
            author(),
            at,
            &policy(),
        )
        .unwrap()
        .0
    }

    fn content() -> ContentDraft {
        ContentDraft::titled("deploy prod")
    }

    // --- T1 · Birth (design §3.4) -----------------------------------------

    #[test]
    fn critical_t1_a_publish_without_state_is_born_closed_and_ages_from_creation() {
        let at = now();
        let notification = create(RevisionDraft::simple(content()), at);

        assert_eq!(notification.kind, NotificationKind::Simple);
        assert_eq!(notification.state(), State::Closed);
        assert_eq!(notification.status(), Status::Info);
        assert_eq!(
            notification.closed_at,
            Some(at),
            "a simple notification starts ageing the moment it is published"
        );
        assert_eq!(
            notification.expires_at(),
            Some(at + TimeDuration::days(7)),
            "and takes the channel default retention"
        );
    }

    #[test]
    fn critical_t1_a_publish_with_state_has_a_life_and_cannot_expire_while_it_runs() {
        let at = now();
        let notification = create(RevisionDraft::lifecycle(State::Open, content()), at);

        assert_eq!(notification.kind, NotificationKind::Lifecycle);
        assert_eq!(notification.state(), State::Open);
        assert_eq!(notification.closed_at, None);
        assert_eq!(notification.expires_at(), None);
        assert!(
            !notification.is_expired(at + TimeDuration::days(3650)),
            "an incident that is still open must never be purged out from under its channel"
        );
    }

    #[test]
    fn closing_for_inactivity_starts_the_retention_clock() {
        let at = now();
        let mut notification = create(RevisionDraft::lifecycle(State::Open, content()), at);
        assert_eq!(
            notification.expires_at(),
            None,
            "an open notification has no deadline — that is the whole problem"
        );

        let swept_at = at + TimeDuration::days(7);
        assert!(notification.close(swept_at));

        assert_eq!(notification.closed_at, Some(swept_at));
        assert_eq!(notification.head.state, State::Closed);
        assert_eq!(
            notification.expires_at(),
            Some(swept_at + TimeDuration::days(7)),
            "closing is what makes the channel's TTL apply at all"
        );
        assert_eq!(
            notification.head.status,
            Status::Info,
            "the server closes it, it does not reinterpret what it said"
        );
    }

    /// A sweep that announced a closure it did not perform would emit an event and
    /// a log line for every already-closed row it looked at, every pass, forever.
    #[test]
    fn closing_an_already_closed_notification_changes_nothing() {
        let at = now();
        let mut notification = create(RevisionDraft::lifecycle(State::Closed, content()), at);
        let closed_at = notification.closed_at;

        assert!(!notification.close(at + TimeDuration::days(30)));
        assert_eq!(
            notification.closed_at, closed_at,
            "a second closure must not push retention back"
        );
    }

    // --- T2 · Reopening ---------------------------------------------------

    #[test]
    fn critical_t2_a_closed_notification_still_accepts_revisions() {
        let at = now();
        let mut notification = create(RevisionDraft::lifecycle(State::Open, content()), at);

        // Close it: the clock starts.
        let closed_at = at + TimeDuration::minutes(10);
        notification
            .revise(
                RevisionDraft {
                    state: Some(State::Closed),
                    status: Some(Status::Failure),
                    ..RevisionDraft::simple(content())
                },
                author(),
                closed_at,
                &policy(),
            )
            .unwrap();
        assert_eq!(notification.closed_at, Some(closed_at));
        assert_eq!(
            notification.expires_at(),
            Some(closed_at + TimeDuration::days(7))
        );

        // Amend the closure: a new status must not push the deadline back.
        notification
            .revise(
                RevisionDraft {
                    status: Some(Status::Warning),
                    ..RevisionDraft::simple(content())
                },
                author(),
                closed_at + TimeDuration::minutes(1),
                &policy(),
            )
            .unwrap();
        assert_eq!(notification.status(), Status::Warning);
        assert_eq!(
            notification.closed_at,
            Some(closed_at),
            "amending a closure is not re-closing it"
        );
    }

    #[test]
    fn critical_t2_reopening_suspends_retention_and_restarts_it_at_the_next_close() {
        let at = now();
        let mut notification = create(RevisionDraft::lifecycle(State::Closed, content()), at);
        assert!(notification.expires_at().is_some());

        // The CI re-run pushes onto the same notification.
        let reopened_at = at + TimeDuration::hours(1);
        notification
            .revise(
                RevisionDraft::lifecycle(State::Working, content()),
                author(),
                reopened_at,
                &policy(),
            )
            .unwrap();
        assert_eq!(notification.state(), State::Working);
        assert_eq!(
            notification.expires_at(),
            None,
            "the clock is suspended again"
        );

        let reclosed_at = reopened_at + TimeDuration::minutes(20);
        notification
            .revise(
                RevisionDraft {
                    state: Some(State::Closed),
                    status: Some(Status::Success),
                    ..RevisionDraft::simple(content())
                },
                author(),
                reclosed_at,
                &policy(),
            )
            .unwrap();
        assert_eq!(
            notification.expires_at(),
            Some(reclosed_at + TimeDuration::days(7)),
            "and restarts from zero, not from the first closure"
        );
    }

    #[test]
    fn critical_t2_reopening_a_simple_notification_gives_it_a_life() {
        let at = now();
        let mut notification = create(RevisionDraft::simple(content()), at);
        assert_eq!(notification.kind, NotificationKind::Simple);

        notification
            .revise(
                RevisionDraft::lifecycle(State::Open, content()),
                author(),
                at + TimeDuration::minutes(5),
                &policy(),
            )
            .unwrap();

        assert_eq!(notification.kind, NotificationKind::Lifecycle);
        assert_eq!(notification.expires_at(), None);
    }

    // --- T3 · Last write wins ---------------------------------------------

    #[test]
    fn critical_t3_the_highest_seq_is_the_current_state() {
        let at = now();
        let mut notification = create(RevisionDraft::lifecycle(State::Open, content()), at);

        // Two concurrent publishers, ordered by the server as they arrive.
        for (offset, status) in [(1, Status::Failure), (2, Status::Success)] {
            notification
                .revise(
                    RevisionDraft {
                        status: Some(status),
                        ..RevisionDraft::simple(content())
                    },
                    author(),
                    at + TimeDuration::seconds(offset),
                    &policy(),
                )
                .unwrap();
        }

        assert_eq!(notification.last_seq, 3);
        assert_eq!(notification.status(), Status::Success);
    }

    #[test]
    fn critical_t3_a_late_revision_is_recorded_but_does_not_win() {
        let at = now();
        let mut notification = create(RevisionDraft::lifecycle(State::Open, content()), at);
        let winner = notification
            .revise(
                RevisionDraft {
                    status: Some(Status::Success),
                    ..RevisionDraft::simple(content())
                },
                author(),
                at + TimeDuration::seconds(1),
                &policy(),
            )
            .unwrap()
            .revision;

        // A replay of an older revision, arriving after the one that superseded it.
        let stale = Revision {
            id: RevisionId::generate(),
            seq: 1,
            status: Status::Failure,
            ..winner.clone()
        };
        notification.apply(&stale);

        assert_eq!(notification.head.seq, winner.seq);
        assert_eq!(notification.status(), Status::Success);
        assert_eq!(
            notification.revision_count, 3,
            "the history keeps everything — it is the audit trail"
        );
    }

    // --- T4 · Actions are replaced, never merged ---------------------------

    #[test]
    fn critical_t4_a_revision_replaces_the_action_set_wholesale() {
        use crate::action::{ActionDraft, Actions};

        let at = now();
        let with_buttons = ContentDraft {
            actions: Actions::new(vec![
                ActionDraft::Ack {
                    label: "Ack".to_owned(),
                    key: "ack-incident".to_owned(),
                }
                .try_into()
                .unwrap(),
            ])
            .unwrap(),
            ..content()
        };
        let mut notification = create(RevisionDraft::lifecycle(State::Open, with_buttons), at);
        assert_eq!(notification.head.content.actions.len(), 1);

        // The next revision carries no actions at all: the buttons are gone, not
        // merged with what came before (design §2.3).
        notification
            .revise(
                RevisionDraft::simple(content()),
                author(),
                at + TimeDuration::minutes(1),
                &policy(),
            )
            .unwrap();
        assert!(notification.head.content.actions.is_empty());
    }

    // --- T6 · Retention and the revision ceiling ---------------------------

    #[test]
    fn critical_t6_a_ttl_past_the_channel_ceiling_is_clamped_and_reported() {
        let policy = ChannelPolicy {
            ttl: TtlPolicy {
                max: Some(HumanDuration::from_secs_const(24 * 3_600)),
                ..TtlPolicy::default()
            },
            ..ChannelPolicy::default()
        };
        let at = now();

        let (notification, written) = Notification::create(
            NotificationId::generate(),
            channel(),
            None,
            RevisionDraft {
                ttl: Some(Ttl::parse("30d", "ttl").unwrap()),
                ..RevisionDraft::simple(content())
            },
            author(),
            at,
            &policy,
        )
        .unwrap();

        assert_eq!(
            written.retention.ttl,
            Ttl::For(HumanDuration::from_secs_const(24 * 3_600))
        );
        assert!(written.retention.clamped_from.is_some());
        assert_eq!(
            notification.expires_at(),
            Some(at + TimeDuration::hours(24))
        );
    }

    #[test]
    fn critical_t6_infinite_retention_needs_the_channel_to_allow_it() {
        let at = now();
        let refused = Notification::create(
            NotificationId::generate(),
            channel(),
            None,
            RevisionDraft {
                ttl: Some(Ttl::parse("0", "ttl").unwrap()),
                ..RevisionDraft::simple(content())
            },
            author(),
            at,
            &policy(),
        );
        assert_eq!(
            refused.unwrap_err(),
            DomainError::InfiniteTtlNotAllowed,
            "a publisher must not be able to pin a channel's storage on its own"
        );
    }

    #[test]
    fn critical_t6_the_thousand_and_first_revision_is_refused_and_the_notification_survives() {
        let at = now();
        let mut notification = create(RevisionDraft::lifecycle(State::Open, content()), at);

        for seq in 2..=MAX_REVISIONS {
            notification
                .revise(
                    RevisionDraft::simple(content()),
                    author(),
                    at + TimeDuration::seconds(i64::from(seq)),
                    &policy(),
                )
                .unwrap();
        }
        assert_eq!(notification.last_seq, MAX_REVISIONS);

        let refused = notification.revise(
            RevisionDraft::simple(ContentDraft::titled("one too many")),
            author(),
            at + TimeDuration::hours(1),
            &policy(),
        );
        assert_eq!(
            refused.unwrap_err(),
            DomainError::RevisionLimitReached { max: MAX_REVISIONS }
        );
        assert_eq!(notification.last_seq, MAX_REVISIONS);
        assert_eq!(
            notification.head.content.title, "deploy prod",
            "the refused revision must leave no trace"
        );
    }

    // --- Deletion ---------------------------------------------------------

    #[test]
    fn a_deleted_notification_accepts_nothing_further() {
        let at = now();
        let mut notification = create(RevisionDraft::lifecycle(State::Open, content()), at);
        notification.delete(at);

        let refused = notification.revise(
            RevisionDraft::simple(content()),
            author(),
            at + TimeDuration::seconds(1),
            &policy(),
        );
        assert_eq!(refused.unwrap_err(), DomainError::NotificationGone);
        assert!(notification.is_gone(at));
    }

    #[test]
    fn deleting_twice_does_not_move_the_tombstone() {
        let at = now();
        let mut notification = create(RevisionDraft::simple(content()), at);
        notification.delete(at);
        notification.delete(at + TimeDuration::hours(1));
        assert_eq!(notification.deleted_at, Some(at));
    }

    #[test]
    fn a_notification_round_trips_through_json() {
        let notification = create(RevisionDraft::lifecycle(State::Open, content()), now());
        let json = serde_json::to_string(&notification).unwrap();
        assert_eq!(
            serde_json::from_str::<Notification>(&json).unwrap(),
            notification
        );
    }
}
