//! Background work: closing what nobody is attending to (design §7.2bis).
//!
//! # Why this job has to exist
//!
//! A lifecycle notification does not age until it closes, and until this job there
//! was nothing in the system that ever closed one. So the retention a channel
//! configured applied to simple notifications and silently did not apply to
//! lifecycle ones: a publisher that crashed, was decommissioned, or just forgot the
//! final call left a row that would outlive the deployment that produced it.
//!
//! The symptom is not "storage grows". It is an inbox that keeps showing a deploy
//! that failed a week ago as though it were still running, which is worse, because
//! a person reading it cannot tell the difference between "still going" and
//! "abandoned" — and the whole promise of a lifecycle notification is that its state
//! means something.
//!
//! # Leader-only, and re-checked between batches
//!
//! Same shape and same reason as [`PurgeExpired`](crate::PurgeExpired): a replica
//! that took the lock, lost it during a partition and kept closing notifications is
//! the failure this guards against (scenario T22). Closing is less destructive than
//! deleting — a wrongly closed notification is reopened by its next revision, which
//! is exactly what a publisher coming back to life sends — but "less destructive"
//! is not a reason to run two of these against one another.

use std::sync::Arc;

use notify_domain::ChannelId;
use notify_ports::{
    ChannelRepository, Clock, EventStore, Failure, LeaderElection, NotificationRepository, Poke,
};
use time::OffsetDateTime;

/// How many to close per batch. Same size and same reasoning as the purge's:
/// small enough that losing leadership is noticed quickly, large enough that a
/// backlog of abandoned notifications does not take all day.
const BATCH: u32 = 200;

/// What one pass did, for the log and for the tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClosureReport {
    /// Notifications closed for want of activity.
    pub closed: u64,
    /// Channels whose policy asked for anything at all — the denominator behind
    /// `closed`, and the number that says "nothing was closed because nothing was
    /// due" apart from "nothing was closed because every channel opted out".
    pub channels_watched: u64,
    /// Whether this replica was the leader. `false` means the pass did nothing and
    /// that this is the expected outcome on all but one replica.
    pub led: bool,
}

/// The periodic stale-closure job (design §7.2bis).
#[derive(Clone)]
pub struct CloseStale {
    notifications: Arc<dyn NotificationRepository>,
    channels: Arc<dyn ChannelRepository>,
    events: Arc<dyn EventStore>,
    election: Arc<dyn LeaderElection>,
    bus: Arc<dyn notify_ports::EventBus>,
    clock: Arc<dyn Clock>,
}

impl core::fmt::Debug for CloseStale {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CloseStale").finish_non_exhaustive()
    }
}

impl CloseStale {
    /// The name this job takes its lock under.
    ///
    /// Its own lock rather than the purge's: the two run on the same timer and one
    /// feeds the other, but a slow purge must not stop closures from happening, and
    /// a shared lock would make them serialise across the whole fleet for no reason.
    pub const JOB: &'static str = "close-stale";

    /// Wire it.
    #[must_use]
    pub fn new(
        notifications: Arc<dyn NotificationRepository>,
        channels: Arc<dyn ChannelRepository>,
        events: Arc<dyn EventStore>,
        election: Arc<dyn LeaderElection>,
        bus: Arc<dyn notify_ports::EventBus>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            notifications,
            channels,
            events,
            election,
            bus,
            clock,
        }
    }

    /// Run one pass.
    pub async fn execute(&self) -> Result<ClosureReport, Failure> {
        let Some(guard) = self.election.try_acquire(Self::JOB).await? else {
            // Not leading is a normal answer, not a failure.
            return Ok(ClosureReport::default());
        };

        let now = self.clock.now();
        let deadlines = self.deadlines(now).await?;
        let mut report = ClosureReport {
            led: true,
            channels_watched: deadlines.len() as u64,
            ..ClosureReport::default()
        };

        // Every channel said "never". Asked here rather than left to the repository
        // so the report can tell the two silences apart.
        if deadlines.is_empty() {
            guard.release().await;
            return Ok(report);
        }

        loop {
            // Between batches, not once at the top — see the module doc.
            if !guard.is_held().await {
                tracing::warn!("lost leadership mid-closure; stopping");
                break;
            }

            let closed = self
                .notifications
                .close_stale(&deadlines, now, BATCH)
                .await?;
            if closed.is_empty() {
                break;
            }
            report.closed += closed.len() as u64;

            // Tell the instance, so a connected client marks its copy closed
            // instead of showing a week-old deploy as still running. One poke per
            // channel per batch: a poke means "go read from your cursor", so a
            // second one for the same channel would only send its streams to read
            // the same rows again — and one head read covers the whole batch.
            let cursor = self.events.head().await?;
            let mut channels: Vec<&ChannelId> = closed
                .iter()
                .map(|notification| &notification.channel)
                .collect();
            channels.sort_unstable();
            channels.dedup();
            for channel in channels {
                let poke = Poke::Channel {
                    id: channel.clone(),
                    cursor,
                };
                if let Err(error) = self.bus.publish(&poke).await {
                    tracing::warn!(?error, "could not announce a closure");
                }
            }

            // A short batch means the backlog is drained. Asked this way rather
            // than by looping until empty so a steady trickle costs one extra
            // query rather than two.
            if closed.len() < BATCH as usize {
                break;
            }
        }

        guard.release().await;
        Ok(report)
    }

    /// Turn every channel's `close_after` into the instant its notifications must
    /// have been idle since.
    ///
    /// Channels that opted out are absent rather than present with a far-away
    /// deadline, which is what the repository contract asks for: "no deadline" and
    /// "a deadline in the year 3000" have to be different things, or an opt-out
    /// becomes a bet on arithmetic.
    async fn deadlines(
        &self,
        now: OffsetDateTime,
    ) -> Result<Vec<(ChannelId, OffsetDateTime)>, Failure> {
        Ok(self
            .channels
            .all_policies()
            .await?
            .into_iter()
            .filter_map(|(id, policy)| {
                // `due_at` reads forwards — "active at T is due at T + close_after" —
                // and what is needed here is the inverse: the cutoff a notification's
                // last activity must predate. Subtracting is that inverse, and it is
                // done once per channel rather than once per row.
                let idle_for = policy.close_after.duration()?;
                let cutoff = now.checked_sub(time::Duration::seconds(
                    i64::try_from(idle_for.as_secs()).ok()?,
                ))?;
                Some((id, cutoff))
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use notify_domain::{
        Author, Channel, ChannelPolicy, CloseAfter, ContentDraft, HumanDuration, Notification,
        NotificationId, Principal, RevisionDraft, State, Status, UserId,
    };
    use notify_ports::test_utils::{InMemory, TestClock, TestLeaderElection};
    use time::Duration;

    use super::*;

    fn days(n: u64) -> HumanDuration {
        HumanDuration::from_secs(n * 24 * 3_600, "close_after").unwrap()
    }

    fn alice() -> Principal {
        Principal::User {
            id: UserId::generate(),
        }
    }

    fn job(store: &InMemory, clock: &TestClock, election: TestLeaderElection) -> CloseStale {
        CloseStale::new(
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(election),
            Arc::new(store.clone()),
            Arc::new(clock.clone()),
        )
    }

    fn channel(store: &InMemory, id: &str, close_after: CloseAfter) -> ChannelId {
        let id = ChannelId::parse(id).unwrap();
        let mut channel = Channel::dynamic(id.clone(), None).unwrap();
        channel.policy = ChannelPolicy {
            close_after,
            ..ChannelPolicy::default()
        };
        store.seed_channel(channel);
        id
    }

    /// Publish a lifecycle notification and hand back its id.
    async fn publish(
        store: &InMemory,
        channel: &ChannelId,
        draft: RevisionDraft,
        at: OffsetDateTime,
    ) -> NotificationId {
        let (notification, written) = Notification::create(
            NotificationId::generate(),
            channel.clone(),
            None,
            draft,
            Author::anonymous(alice()),
            at,
            &ChannelPolicy::default(),
        )
        .unwrap();
        NotificationRepository::insert(store, &notification, &written.revision)
            .await
            .unwrap();
        notification.id
    }

    async fn reload(store: &InMemory, id: NotificationId) -> Notification {
        NotificationRepository::get(store, id)
            .await
            .unwrap()
            .unwrap()
    }

    /// The scenario this job was written for, end to end: a lifecycle notification
    /// that reported `failure` and then went quiet.
    #[tokio::test]
    async fn an_abandoned_lifecycle_notification_is_closed_and_starts_ageing() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let channel = channel(&store, "ci-infra", CloseAfter::default());

        let id = publish(
            &store,
            &channel,
            RevisionDraft {
                state: Some(State::Open),
                status: Some(Status::Failure),
                ..RevisionDraft::simple(ContentDraft::titled("Deploy prod-eu-3"))
            },
            clock.now(),
        )
        .await;
        assert_eq!(
            reload(&store, id).await.expires_at(),
            None,
            "an open notification has no deadline — that is the whole problem"
        );

        clock.advance(Duration::days(9));
        let swept_at = clock.now();
        let report = job(&store, &clock, TestLeaderElection::leader())
            .execute()
            .await
            .unwrap();

        assert!(report.led);
        assert_eq!(report.closed, 1);
        assert_eq!(report.channels_watched, 1);

        let closed = reload(&store, id).await;
        assert_eq!(closed.head.state, State::Closed);
        assert_eq!(closed.closed_at, Some(swept_at));
        assert_eq!(
            closed.head.status,
            Status::Failure,
            "the server closes it; it does not reinterpret what it said"
        );
        assert_eq!(
            closed.expires_at(),
            Some(swept_at + Duration::days(7)),
            "which is the point: the channel's TTL finally applies to it"
        );
        assert_eq!(
            store.pokes().len(),
            1,
            "a client showing it as running has to hear about this"
        );
    }

    /// The counterpart, and the one a false positive would break: a publisher that
    /// is still calling keeps its notification open however old it is.
    #[tokio::test]
    async fn a_notification_that_is_still_being_revised_is_left_alone() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let channel = channel(&store, "ci-infra", CloseAfter::default());

        let id = publish(
            &store,
            &channel,
            RevisionDraft::lifecycle(State::Open, ContentDraft::titled("long incident")),
            clock.now(),
        )
        .await;

        // A long incident, still being updated an hour before the sweep.
        clock.advance(Duration::days(30));
        let mut notification = reload(&store, id).await;
        let written = notification
            .revise(
                RevisionDraft::lifecycle(State::Working, ContentDraft::titled("still on it")),
                Author::anonymous(alice()),
                clock.now() - Duration::hours(1),
                &ChannelPolicy::default(),
            )
            .unwrap();
        NotificationRepository::append_revision(&store, &notification, &written.revision)
            .await
            .unwrap();

        let report = job(&store, &clock, TestLeaderElection::leader())
            .execute()
            .await
            .unwrap();

        assert_eq!(report.closed, 0);
        assert_eq!(reload(&store, id).await.head.state, State::Working);
    }

    /// `"0"` has to reach all the way through: a channel that opted out must not
    /// appear in the deadlines at all, whatever the arithmetic would have said.
    #[tokio::test]
    async fn a_channel_that_opted_out_is_never_swept() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let channel = channel(&store, "ci-infra", CloseAfter::Never);

        let id = publish(
            &store,
            &channel,
            RevisionDraft::lifecycle(State::Open, ContentDraft::titled("ours to close")),
            clock.now(),
        )
        .await;

        clock.advance(Duration::days(365));
        let report = job(&store, &clock, TestLeaderElection::leader())
            .execute()
            .await
            .unwrap();

        assert_eq!(report.channels_watched, 0);
        assert_eq!(report.closed, 0);
        assert_eq!(reload(&store, id).await.head.state, State::Open);
    }

    /// Per-channel and not global: a channel with a short fuse must not drag its
    /// neighbour's notifications down with it.
    #[tokio::test]
    async fn each_channel_gets_its_own_deadline() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let impatient = channel(&store, "ci-infra", CloseAfter::After(days(2)));
        let patient = channel(&store, "ops-oncall", CloseAfter::After(days(30)));

        let doomed = publish(
            &store,
            &impatient,
            RevisionDraft::lifecycle(State::Open, ContentDraft::titled("build")),
            clock.now(),
        )
        .await;
        let spared = publish(
            &store,
            &patient,
            RevisionDraft::lifecycle(State::Open, ContentDraft::titled("incident")),
            clock.now(),
        )
        .await;

        clock.advance(Duration::days(3));
        let report = job(&store, &clock, TestLeaderElection::leader())
            .execute()
            .await
            .unwrap();

        assert_eq!(report.closed, 1);
        assert_eq!(report.channels_watched, 2);
        assert_eq!(reload(&store, doomed).await.head.state, State::Closed);
        assert_eq!(reload(&store, spared).await.head.state, State::Open);
    }

    /// A simple notification is born closed, so it is none of this job's business —
    /// and sweeping one would push its retention deadline back by a week every time
    /// the sweep ran.
    #[tokio::test]
    async fn a_simple_notification_is_not_touched() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let channel = channel(&store, "ci-infra", CloseAfter::default());

        let id = publish(
            &store,
            &channel,
            RevisionDraft::simple(ContentDraft::titled("brief")),
            clock.now(),
        )
        .await;
        let deadline = reload(&store, id).await.expires_at();

        clock.advance(Duration::days(3));
        let report = job(&store, &clock, TestLeaderElection::leader())
            .execute()
            .await
            .unwrap();

        assert_eq!(report.closed, 0);
        assert_eq!(
            reload(&store, id).await.expires_at(),
            deadline,
            "its clock started when it was published and must not be restarted"
        );
    }

    #[tokio::test]
    async fn a_follower_does_nothing() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let channel = channel(&store, "ci-infra", CloseAfter::default());
        publish(
            &store,
            &channel,
            RevisionDraft::lifecycle(State::Open, ContentDraft::titled("build")),
            clock.now(),
        )
        .await;
        clock.advance(Duration::days(30));

        let report = job(&store, &clock, TestLeaderElection::follower())
            .execute()
            .await
            .unwrap();

        assert_eq!(report, ClosureReport::default());
        assert!(!report.led);
    }
}
