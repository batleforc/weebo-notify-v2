//! Background work: the TTL purge (§7.2bis, §11). Leader-only, re-checked between
//! batches (T22). The event log outlives expired notifications so slow clients catch up (ADR-0012).

use std::sync::Arc;

use notify_ports::{
    Clock, EventStore, Failure, IdempotencyStore, LeaderElection, NotificationRepository, Poke,
};
use time::{Duration, OffsetDateTime};

/// How much to claim per batch: small enough to notice lost leadership quickly.
const BATCH: u32 = 200;

/// How long a soft-deleted notification waits before its row goes, so a client that
/// was offline still receives the tombstone.
const TOMBSTONE_GRACE: Duration = Duration::days(7);

/// How long events outlive the notifications they describe.
const EVENT_RETENTION: Duration = Duration::days(30);

/// How long a signed upload may sit unreferenced before it is collected (§7.2bis).
/// Covers the gap between signing and publishing; the URL itself lives 15 minutes.
const PRESIGN_GRACE: Duration = Duration::days(1);

/// What one pass did, for the log and for the tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PurgeReport {
    /// Notifications whose retention ran out.
    pub expired: u64,
    /// Rows hard-deleted after their grace period.
    pub purged: u64,
    /// Attachment objects deleted because only purged rows still referenced them.
    pub attachments_deleted: u64,
    /// Signed uploads collected because nothing ever referenced them (a client bug or
    /// abuse, not retention).
    pub presigns_collected: u64,
    /// Event rows dropped.
    pub events_pruned: u64,
    /// Idempotency reservations dropped.
    pub reservations_pruned: u64,
    /// Whether this replica actually led.
    pub led: bool,
}

/// The periodic purge job (design §7.2bis).
#[derive(Clone)]
pub struct PurgeExpired {
    notifications: Arc<dyn NotificationRepository>,
    events: Arc<dyn EventStore>,
    idempotency: Arc<dyn IdempotencyStore>,
    attachments: crate::attachments::Attachments,
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

    #[must_use]
    pub fn new(
        notifications: Arc<dyn NotificationRepository>,
        events: Arc<dyn EventStore>,
        idempotency: Arc<dyn IdempotencyStore>,
        attachments: crate::attachments::Attachments,
        election: Arc<dyn LeaderElection>,
        bus: Arc<dyn notify_ports::EventBus>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            notifications,
            events,
            idempotency,
            attachments,
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
            // Between batches: a partition mid-run must stop this replica (T22).
            if !guard.is_held().await {
                tracing::warn!("lost leadership mid-purge; stopping");
                break;
            }

            let claimed = self.notifications.claim_expired(now, BATCH).await?;
            if claimed.is_empty() {
                break;
            }
            report.expired += claimed.len() as u64;

            // So connected clients drop what expired.
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
            let cutoff = now - TOMBSTONE_GRACE;

            // Objects before rows (§7.2bis, §6.1): the rows are the only record of which object
            // belongs to what, and a bucket outage just leaves them for the next pass.
            let swept = self.sweep_attachments(cutoff).await?;
            report.attachments_deleted = swept.deleted;

            // Only once the bucket is clean, or the failed object becomes unfindable.
            if swept.failed == 0 {
                report.purged = self.notifications.purge(cutoff, BATCH).await?;
            } else {
                tracing::warn!(
                    failed = swept.failed,
                    "holding the row purge back until the object store accepts its deletes"
                );
            }

            // The other half: objects that never had a reference, which only the registry knows.
            report.presigns_collected = self.collect_unreferenced(now - PRESIGN_GRACE).await?;

            // Not gated on the sweep: a bucket outage must not stop event pruning.
            report.events_pruned = self.events.prune(now - EVENT_RETENTION, BATCH).await?;
            report.reservations_pruned = self.idempotency.prune(now).await?;
        }

        guard.release().await;
        Ok(report)
    }

    /// Delete the objects that only rows due for purge still reference. A failed delete
    /// is counted, logged, and holds the row purge back so the next pass retries it.
    async fn sweep_attachments(&self, cutoff: OffsetDateTime) -> Result<Swept, Failure> {
        let orphaned = self
            .notifications
            .orphaned_attachments(cutoff, BATCH)
            .await?;

        let mut swept = Swept::default();
        for id in orphaned {
            match self.attachments.store.delete(id).await {
                Ok(()) => swept.deleted += 1,
                Err(error) => {
                    swept.failed += 1;
                    tracing::warn!(
                        %error,
                        attachment = %id,
                        "could not delete an expired attachment; its rows stay for the next pass"
                    );
                }
            }
        }
        Ok(swept)
    }

    /// Delete objects that were signed for and never referenced; the registry row goes
    /// only when its object did.
    async fn collect_unreferenced(&self, older_than: OffsetDateTime) -> Result<u64, Failure> {
        let stale = self
            .attachments
            .registry
            .unreferenced(older_than, BATCH)
            .await?;

        let mut collected = Vec::new();
        for id in stale {
            match self.attachments.store.delete(id).await {
                Ok(()) => collected.push(id),
                Err(error) => tracing::warn!(
                    %error,
                    attachment = %id,
                    "could not delete an unreferenced upload; it stays for the next pass"
                ),
            }
        }

        self.attachments.registry.forget(&collected).await?;
        Ok(collected.len() as u64)
    }
}

/// What one attachment sweep managed.
#[derive(Debug, Default, Clone, Copy)]
struct Swept {
    deleted: u64,
    failed: u64,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use notify_domain::{
        Author, Channel, ChannelId, ChannelPolicy, ContentDraft, Membership, Notification,
        NotificationId, Principal, RevisionDraft, Scope, UserId,
    };
    use notify_ports::test_utils::{InMemory, InMemoryAttachments, TestClock, TestLeaderElection};

    use super::*;

    fn channel_id() -> ChannelId {
        ChannelId::parse("ci-infra").unwrap()
    }

    fn job(store: &InMemory, clock: &TestClock, election: TestLeaderElection) -> PurgeExpired {
        with_attachments(store, clock, election, InMemoryAttachments::new())
    }

    fn with_attachments(
        store: &InMemory,
        clock: &TestClock,
        election: TestLeaderElection,
        attachments: InMemoryAttachments,
    ) -> PurgeExpired {
        PurgeExpired::new(
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            crate::attachments::Attachments {
                store: Arc::new(attachments),
                registry: Arc::new(store.clone()),
            },
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

    // --- Attachments (design §7.2bis, §6.1) -------------------------------

    /// Publish a notification carrying an attachment image, and mark it uploaded.
    async fn seed_with_attachment(
        store: &InMemory,
        clock: &TestClock,
        alice: &Principal,
        bucket: &InMemoryAttachments,
    ) -> notify_domain::AttachmentId {
        let reference = notify_domain::AttachmentId::generate();
        bucket.mark_uploaded(reference);

        let (notification, written) = Notification::create(
            NotificationId::generate(),
            channel_id(),
            None,
            RevisionDraft::simple(ContentDraft {
                image: Some(notify_domain::Image::Attachment { reference }),
                ..ContentDraft::titled("with a screenshot")
            }),
            Author::anonymous(alice.clone()),
            clock.now(),
            &ChannelPolicy::default(),
        )
        .unwrap();
        NotificationRepository::insert(store, &notification, &written.revision)
            .await
            .unwrap();
        reference
    }

    #[tokio::test]
    async fn an_expired_notifications_attachment_leaves_the_bucket_with_it() {
        // Every published attachment must go with its notification's row.
        let store = InMemory::new();
        let clock = TestClock::new();
        let bucket = InMemoryAttachments::new();
        let alice = Principal::User {
            id: UserId::generate(),
        };
        let mut channel = Channel::dynamic(channel_id(), None).unwrap();
        channel.upsert_member(Membership::new(alice.clone(), vec![Scope::Subscribe]).unwrap());
        store.seed_channel(channel);

        let reference = seed_with_attachment(&store, &clock, &alice, &bucket).await;
        assert!(bucket.uploaded().contains(&reference));

        // Past the TTL and the tombstone grace: the object goes with the row.
        clock.advance(Duration::days(8));
        let job = with_attachments(&store, &clock, TestLeaderElection::leader(), bucket.clone());
        job.execute().await.unwrap();

        clock.advance(TOMBSTONE_GRACE + Duration::days(1));
        let report = job.execute().await.unwrap();

        assert_eq!(report.attachments_deleted, 1);
        assert!(
            !bucket.uploaded().contains(&reference),
            "the object outlived the notification that was its only reason to exist"
        );
    }

    #[tokio::test]
    async fn an_attachment_a_live_notification_still_uses_is_left_alone() {
        // One attachment may be named by two notifications; deleting a live one's image
        // would be data loss.
        let store = InMemory::new();
        let clock = TestClock::new();
        let bucket = InMemoryAttachments::new();
        let alice = Principal::User {
            id: UserId::generate(),
        };
        let mut channel = Channel::dynamic(channel_id(), None).unwrap();
        channel.upsert_member(Membership::new(alice.clone(), vec![Scope::Subscribe]).unwrap());
        store.seed_channel(channel);

        let shared = seed_with_attachment(&store, &clock, &alice, &bucket).await;

        // A second notification naming the same object, whose TTL has not run out.
        clock.advance(Duration::days(6));
        let (second, written) = Notification::create(
            NotificationId::generate(),
            channel_id(),
            None,
            RevisionDraft::simple(ContentDraft {
                image: Some(notify_domain::Image::Attachment { reference: shared }),
                ..ContentDraft::titled("same screenshot, still current")
            }),
            Author::anonymous(alice.clone()),
            clock.now(),
            &ChannelPolicy::default(),
        )
        .unwrap();
        NotificationRepository::insert(&store, &second, &written.revision)
            .await
            .unwrap();

        clock.advance(Duration::days(2) + TOMBSTONE_GRACE);
        let job = with_attachments(&store, &clock, TestLeaderElection::leader(), bucket.clone());
        job.execute().await.unwrap();
        let report = job.execute().await.unwrap();

        assert_eq!(report.attachments_deleted, 0);
        assert!(
            bucket.uploaded().contains(&shared),
            "a notification that is still live still needs its image"
        );
    }

    #[tokio::test]
    async fn a_notification_with_no_attachment_sweeps_nothing() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let alice = Principal::User {
            id: UserId::generate(),
        };
        seed(&store, &clock, &alice).await;

        // Expiry, then the tombstone grace.
        clock.advance(Duration::days(8));
        let job = job(&store, &clock, TestLeaderElection::leader());
        job.execute().await.unwrap();

        clock.advance(TOMBSTONE_GRACE + Duration::days(1));
        let report = job.execute().await.unwrap();

        assert_eq!(report.attachments_deleted, 0);
        assert!(report.purged > 0, "the rows still go");
    }

    #[tokio::test]
    async fn an_upload_nobody_ever_published_is_collected() {
        // Presign, upload, never publish: only the registry row can find it.
        let store = InMemory::new();
        let clock = TestClock::new();
        let bucket = InMemoryAttachments::new();
        let orphan = notify_domain::AttachmentId::generate();
        bucket.mark_uploaded(orphan);
        notify_ports::AttachmentRegistry::record(&store, orphan, &channel_id(), clock.now())
            .await
            .unwrap();

        clock.advance(PRESIGN_GRACE + Duration::hours(1));
        let report = with_attachments(&store, &clock, TestLeaderElection::leader(), bucket.clone())
            .execute()
            .await
            .unwrap();

        assert_eq!(report.presigns_collected, 1);
        assert!(!bucket.uploaded().contains(&orphan));
        assert!(
            notify_ports::AttachmentRegistry::unreferenced(
                &store,
                clock.now() + Duration::days(1),
                10
            )
            .await
            .unwrap()
            .is_empty(),
            "the row goes with the object, or the registry grows for ever"
        );
    }

    #[tokio::test]
    async fn an_upload_still_inside_its_grace_period_is_left_alone() {
        // Between signing and publishing: collecting here would destroy an upload in flight.
        let store = InMemory::new();
        let clock = TestClock::new();
        let bucket = InMemoryAttachments::new();
        let in_flight = notify_domain::AttachmentId::generate();
        bucket.mark_uploaded(in_flight);
        notify_ports::AttachmentRegistry::record(&store, in_flight, &channel_id(), clock.now())
            .await
            .unwrap();

        clock.advance(Duration::hours(1));
        let report = with_attachments(&store, &clock, TestLeaderElection::leader(), bucket.clone())
            .execute()
            .await
            .unwrap();

        assert_eq!(report.presigns_collected, 0);
        assert!(
            bucket.uploaded().contains(&in_flight),
            "an upload signed an hour ago is a publisher still working, not an orphan"
        );
    }

    #[tokio::test]
    async fn a_published_attachment_is_never_collected_as_unreferenced() {
        // Only the revisions answer whether a signed upload is still wanted, however old.
        let store = InMemory::new();
        let clock = TestClock::new();
        let bucket = InMemoryAttachments::new();
        let alice = Principal::User {
            id: UserId::generate(),
        };
        let mut channel = Channel::dynamic(channel_id(), None).unwrap();
        channel.upsert_member(Membership::new(alice.clone(), vec![Scope::Subscribe]).unwrap());
        store.seed_channel(channel);

        let reference = seed_with_attachment(&store, &clock, &alice, &bucket).await;
        notify_ports::AttachmentRegistry::record(&store, reference, &channel_id(), clock.now())
            .await
            .unwrap();

        clock.advance(PRESIGN_GRACE + Duration::hours(1));
        let report = with_attachments(&store, &clock, TestLeaderElection::leader(), bucket.clone())
            .execute()
            .await
            .unwrap();

        assert_eq!(report.presigns_collected, 0);
        assert!(bucket.uploaded().contains(&reference));
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
