//! Background work: the TTL purge (design §7.2bis, §11).
//!
//! Leader-only, and it re-checks that between batches. A replica that took the lock,
//! lost it during a partition and kept deleting is the failure this exists to
//! prevent (scenario T22) — losing the lock is only useful if somebody looks.
//!
//! Two retentions, deliberately different. A notification's TTL is a product
//! decision. The **event log's** retention is an operational one: the events of an
//! expired notification have to outlive it long enough for a slow client to catch
//! up, and only then go (ADR-0012).

use std::sync::Arc;

use notify_ports::{
    Clock, EventStore, Failure, IdempotencyStore, LeaderElection, NotificationRepository, Poke,
};
use time::Duration;

/// How much to claim per batch. Small enough that losing leadership is noticed
/// quickly, large enough that a big backlog does not take all day.
const BATCH: u32 = 200;

/// How long a soft-deleted notification waits before its row goes, so a client that
/// was offline still receives the tombstone.
const TOMBSTONE_GRACE: Duration = Duration::days(7);

/// How long events outlive the notifications they describe.
const EVENT_RETENTION: Duration = Duration::days(30);

/// What one pass did, for the log and for the tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PurgeReport {
    /// Notifications whose retention ran out.
    pub expired: u64,
    /// Rows hard-deleted after their grace period.
    pub purged: u64,
    /// Event rows dropped.
    pub events_pruned: u64,
    /// Idempotency reservations dropped.
    pub reservations_pruned: u64,
    /// Whether this replica actually led. `false` means another one is doing it,
    /// which is the normal case on all but one replica.
    pub led: bool,
}

/// The periodic purge job (design §7.2bis).
#[derive(Clone)]
pub struct PurgeExpired {
    notifications: Arc<dyn NotificationRepository>,
    events: Arc<dyn EventStore>,
    idempotency: Arc<dyn IdempotencyStore>,
    election: Arc<dyn LeaderElection>,
    bus: Arc<dyn notify_ports::EventBus>,
    clock: Arc<dyn Clock>,
}

impl core::fmt::Debug for PurgeExpired {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PurgeExpired").finish_non_exhaustive()
    }
}

impl PurgeExpired {
    /// The name this job takes its lock under.
    pub const JOB: &'static str = "purge";

    /// Wire it.
    #[must_use]
    pub fn new(
        notifications: Arc<dyn NotificationRepository>,
        events: Arc<dyn EventStore>,
        idempotency: Arc<dyn IdempotencyStore>,
        election: Arc<dyn LeaderElection>,
        bus: Arc<dyn notify_ports::EventBus>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            notifications,
            events,
            idempotency,
            election,
            bus,
            clock,
        }
    }

    /// Run one pass.
    pub async fn execute(&self) -> Result<PurgeReport, Failure> {
        let Some(guard) = self.election.try_acquire(Self::JOB).await? else {
            // Not leading is a normal answer, not a failure.
            return Ok(PurgeReport::default());
        };

        let now = self.clock.now();
        let mut report = PurgeReport {
            led: true,
            ..PurgeReport::default()
        };

        loop {
            // Between batches, not once at the top: a partition mid-run must stop
            // this replica before it deletes anything else (scenario T22).
            if !guard.is_held().await {
                tracing::warn!("lost leadership mid-purge; stopping");
                break;
            }

            let claimed = self.notifications.claim_expired(now, BATCH).await?;
            if claimed.is_empty() {
                break;
            }
            report.expired += claimed.len() as u64;

            // Tell the instance, so connected clients drop what expired rather than
            // showing a notification that no longer exists.
            for expired in &claimed {
                let cursor = self.events.head().await?;
                let poke = Poke::Channel {
                    id: expired.channel.clone(),
                    cursor,
                };
                if let Err(error) = self.bus.publish(&poke).await {
                    tracing::warn!(?error, "could not announce an expiry");
                }
            }

            if claimed.len() < BATCH as usize {
                break;
            }
        }

        if guard.is_held().await {
            report.purged = self
                .notifications
                .purge(now - TOMBSTONE_GRACE, BATCH)
                .await?;
            report.events_pruned = self.events.prune(now - EVENT_RETENTION, BATCH).await?;
            report.reservations_pruned = self.idempotency.prune(now).await?;
        }

        guard.release().await;
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use notify_domain::{
        Author, Channel, ChannelId, ChannelPolicy, ContentDraft, Membership, Notification,
        NotificationId, Principal, RevisionDraft, Scope, UserId,
    };
    use notify_ports::test_utils::{InMemory, TestClock, TestLeaderElection};

    use super::*;

    fn channel_id() -> ChannelId {
        ChannelId::parse("ci-infra").unwrap()
    }

    fn job(store: &InMemory, clock: &TestClock, election: TestLeaderElection) -> PurgeExpired {
        PurgeExpired::new(
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(election),
            Arc::new(store.clone()),
            Arc::new(clock.clone()),
        )
    }

    async fn seed(store: &InMemory, clock: &TestClock, alice: &Principal) {
        let mut channel = Channel::dynamic(channel_id(), None).unwrap();
        channel.upsert_member(Membership::new(alice.clone(), vec![Scope::Subscribe]).unwrap());
        store.seed_channel(channel);

        let (notification, written) = Notification::create(
            NotificationId::generate(),
            channel_id(),
            None,
            // Simple: it ages from creation.
            RevisionDraft::simple(ContentDraft::titled("brief")),
            Author::anonymous(alice.clone()),
            clock.now(),
            &ChannelPolicy::default(),
        )
        .unwrap();
        NotificationRepository::insert(store, &notification, &written.revision)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_follower_does_nothing_at_all() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let alice = Principal::User {
            id: UserId::generate(),
        };
        seed(&store, &clock, &alice).await;
        clock.advance(Duration::days(8));

        let report = job(&store, &clock, TestLeaderElection::follower())
            .execute()
            .await
            .unwrap();

        assert_eq!(report, PurgeReport::default());
        assert!(!report.led);
    }

    #[tokio::test]
    async fn the_leader_expires_what_is_due_and_announces_it() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let alice = Principal::User {
            id: UserId::generate(),
        };
        seed(&store, &clock, &alice).await;
        clock.advance(Duration::days(8));

        let report = job(&store, &clock, TestLeaderElection::leader())
            .execute()
            .await
            .unwrap();

        assert_eq!(report.expired, 1);
        assert!(report.led);
        assert_eq!(store.pokes().len(), 1);
    }

    #[tokio::test]
    async fn nothing_due_is_not_an_error() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let alice = Principal::User {
            id: UserId::generate(),
        };
        seed(&store, &clock, &alice).await;

        let report = job(&store, &clock, TestLeaderElection::leader())
            .execute()
            .await
            .unwrap();
        assert_eq!(report.expired, 0);
    }

    // --- T22 · Leadership (design §3.4) -----------------------------------

    #[tokio::test]
    async fn critical_t22_losing_leadership_stops_the_job_before_it_cleans_up() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let alice = Principal::User {
            id: UserId::generate(),
        };
        seed(&store, &clock, &alice).await;
        clock.advance(Duration::days(40));

        let election = TestLeaderElection::leader();
        let job = job(&store, &clock, election.clone());
        // A partition takes the lock away before the pass begins.
        election.depose();

        let report = job.execute().await.unwrap();

        assert!(
            !report.led,
            "a deposed replica must not carry on deleting rows another one now owns"
        );
        assert_eq!(report.events_pruned, 0);
    }
}
