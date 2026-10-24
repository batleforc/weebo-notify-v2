//! Outbound sinks (§7.1bis, §10.4bis): the leader's drain loop and the two DLQ
//! operations. A publish only queues (see [`crate::fanout::Fanout`]) so a consumer's
//! latency never reaches a publisher. Schedule: [`notify_domain::RetryPolicy`];
//! classification: the adapter's [`SinkOutcome`].

use std::sync::Arc;

use futures::StreamExt as _;
use notify_domain::{ChannelId, DeliveryId, RetryPolicy};
use notify_ports::{
    AuditEntry, AuditLog, Clock, DeadLetter, Failure, LeaderElection, LeaderGuard, SinkDelivery,
    SinkDispatcher, SinkOutcome, SinkQueue, SinkRegistry, actions,
};

use crate::authz::Caller;

/// How many deliveries one pass takes on; bounded so a lost lock is noticed (T22).
const BATCH: u32 = 100;

/// How many sinks are spoken to at once. Concurrency is across sinks, never within one:
/// a sink's deliveries go in claim order, and one slow endpoint is never hit in parallel.
const CONCURRENCY: usize = 8;

/// How long a pass keeps starting attempts; what is still claimed after is released for
/// the next pass, bounding a pass at this plus one attempt's own timeout.
const PASS_BUDGET: std::time::Duration = std::time::Duration::from_secs(60);

/// How long a claimed delivery stays claimed: past the worst pass so a working drain
/// never loses it, short enough that a killed leader's delivery is owed again within minutes.
const LEASE: time::Duration = time::Duration::minutes(5);

/// How long a delivery whose sink this replica does not know waits before it is looked
/// at again: during a rolling deploy another replica may know it.
const ORPHAN_RECHECK: time::Duration = time::Duration::minutes(5);

/// How many expired dead letters one pass forgets.
const PRUNE_BATCH: u32 = 500;

/// What one drain pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DrainReport {
    /// Deliveries the consumer accepted.
    pub delivered: u64,
    /// Deliveries that failed and are scheduled for another try.
    pub retried: u64,
    /// Deliveries that became dead letters this pass.
    pub buried: u64,
    /// Deliveries whose sink this replica's configuration does not declare, put
    /// back for later rather than dropped (see [`DrainSinks`]).
    pub orphaned: u64,
    /// Dead letters forgotten because they outlived the retention.
    pub pruned: u64,
    /// Whether this replica led. `false` is the normal answer on all but one.
    pub led: bool,
}

impl DrainReport {
    fn absorb(&mut self, other: Self) {
        self.delivered = self.delivered.saturating_add(other.delivered);
        self.retried = self.retried.saturating_add(other.retried);
        self.buried = self.buried.saturating_add(other.buried);
        self.orphaned = self.orphaned.saturating_add(other.orphaned);
        self.pruned = self.pruned.saturating_add(other.pruned);
    }
}

/// The periodic sink drain (design §7.1bis, §11 — leader-only).
///
/// # A sink this replica does not know is not a sink that is gone
///
/// Each replica holds its own copy of the configuration, and during a rolling
/// deploy or after a `SIGHUP` that reached only some pods those copies differ. A
/// delivery whose sink is missing from *this* replica's routes may well be one the
/// next leader can send — so it is never completed here. It is put back with a
/// delay, its give-up clock started, and only when the normal retry window has run
/// out is it buried, with a reason that says so. A sink an operator really removed
/// therefore ends in the dead-letter queue a day later, where it can be seen and
/// dropped, rather than vanishing the moment some replica with an old file led.
#[derive(Clone)]
pub struct DrainSinks {
    queue: Arc<dyn SinkQueue>,
    registry: Arc<dyn SinkRegistry>,
    dispatcher: Arc<dyn SinkDispatcher>,
    election: Arc<dyn LeaderElection>,
    clock: Arc<dyn Clock>,
    policy: RetryPolicy,
    dead_letter_retention: Option<time::Duration>,
}

impl core::fmt::Debug for DrainSinks {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DrainSinks").finish_non_exhaustive()
    }
}

impl DrainSinks {
    /// The name this job takes its lock under.
    pub const JOB: &'static str = "sink-drain";

    #[must_use]
    pub fn new(
        queue: Arc<dyn SinkQueue>,
        registry: Arc<dyn SinkRegistry>,
        dispatcher: Arc<dyn SinkDispatcher>,
        election: Arc<dyn LeaderElection>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            queue,
            registry,
            dispatcher,
            election,
            clock,
            policy: RetryPolicy::CONTRACT,
            dead_letter_retention: None,
        }
    }

    /// Override the retry schedule (tests only).
    #[must_use]
    pub const fn with_policy(mut self, policy: RetryPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Forget dead letters older than `retention` at the end of each pass it leads
    /// (`[sinks] dead_letter_retention`). Without it they are kept for ever.
    #[must_use]
    pub const fn with_dead_letter_retention(mut self, retention: time::Duration) -> Self {
        self.dead_letter_retention = Some(retention);
        self
    }

    /// Run one pass.
    pub async fn execute(&self) -> Result<DrainReport, Failure> {
        let Some(guard) = self.election.try_acquire(Self::JOB).await? else {
            return Ok(DrainReport::default());
        };

        let outcome = self.drain(guard.as_ref()).await;
        guard.release().await;
        outcome
    }

    /// The pass itself, under a lock `execute` took and will release.
    async fn drain(&self, guard: &dyn LeaderGuard) -> Result<DrainReport, Failure> {
        let started = std::time::Instant::now();
        let now = self.clock.now();
        let mut report = DrainReport {
            led: true,
            ..DrainReport::default()
        };

        // One lane per sink, in claim order — see `CONCURRENCY`.
        let mut lanes: Vec<Vec<SinkDelivery>> = Vec::new();
        for delivery in self
            .queue
            .claim_due(now, BATCH, now.saturating_add(LEASE))
            .await?
        {
            match lanes.iter_mut().find(|lane| {
                lane.first().is_some_and(|first| {
                    first.channel == delivery.channel && first.sink == delivery.sink
                })
            }) {
                Some(lane) => lane.push(delivery),
                None => lanes.push(vec![delivery]),
            }
        }

        let outcomes: Vec<Result<DrainReport, Failure>> = futures::stream::iter(lanes)
            .map(|lane| self.lane(lane, guard, started))
            .buffer_unordered(CONCURRENCY)
            .collect()
            .await;
        // Every lane finishes before the first error is reported, so nothing is abandoned.
        for outcome in outcomes {
            report.absorb(outcome?);
        }

        if let Some(retention) = self.dead_letter_retention
            && guard.is_held().await
        {
            report.pruned = self
                .queue
                .prune_dead_letters(self.clock.now().saturating_sub(retention), PRUNE_BATCH)
                .await?;
        }

        Ok(report)
    }

    /// One sink's deliveries, one at a time.
    async fn lane(
        &self,
        lane: Vec<SinkDelivery>,
        guard: &dyn LeaderGuard,
        started: std::time::Instant,
    ) -> Result<DrainReport, Failure> {
        let mut report = DrainReport::default();
        let mut rest = lane.into_iter();

        while let Some(delivery) = rest.next() {
            // Re-checked per delivery: a partition mid-pass must stop this replica (T22).
            if !guard.is_held().await {
                tracing::warn!("lost leadership mid-drain; stopping");
                break;
            }

            if started.elapsed() > PASS_BUDGET {
                // Out of time: hand the rest back now rather than when the lease
                // runs out, so the next pass starts on them in seconds.
                self.release(std::iter::once(delivery).chain(rest), self.clock.now())
                    .await?;
                break;
            }

            if self.attempt(&delivery, &mut report).await? == Endpoint::Failing {
                // The endpoint itself is down or slow. Every other delivery in this
                // lane would wait out the same timeout, so they are released to be
                // tried after the first backoff rather than one by one now.
                let later = self
                    .clock
                    .now()
                    .saturating_add(to_time(self.policy.backoff(1)));
                self.release(rest, later).await?;
                break;
            }
        }

        Ok(report)
    }

    /// Hand claimed deliveries back, untried, due at `at`.
    async fn release(
        &self,
        deliveries: impl Iterator<Item = SinkDelivery>,
        at: time::OffsetDateTime,
    ) -> Result<(), Failure> {
        for delivery in deliveries {
            self.queue.enqueue(&delivery, at).await?;
        }
        Ok(())
    }

    /// How deep the dead-letter queue is, per channel (§13, `sink_dlq_depth`). Only the
    /// leader asks, so the gauge is the leader's answer.
    pub async fn dead_letter_depth(&self) -> Result<Vec<(ChannelId, u64)>, Failure> {
        Ok(self.queue.dead_letter_depth().await?)
    }

    /// One delivery, from resolving its target to writing down what happened.
    async fn attempt(
        &self,
        delivery: &SinkDelivery,
        report: &mut DrainReport,
    ) -> Result<Endpoint, Failure> {
        let Some(target) = self.registry.target(&delivery.channel, &delivery.sink) else {
            self.defer_orphan(delivery, report).await?;
            return Ok(Endpoint::Answering);
        };

        let attempted = SinkDelivery {
            attempts: delivery.attempts.saturating_add(1),
            ..delivery.clone()
        };

        // An unusable dispatcher is infrastructure, not this delivery: retry, don't abort the batch.
        let outcome = match self.dispatcher.deliver(&attempted, &target).await {
            Ok(outcome) => outcome,
            Err(error) => SinkOutcome::Retry {
                reason: error.to_string(),
            },
        };

        let now = self.clock.now();
        match outcome {
            SinkOutcome::Delivered => {
                self.queue.complete(&delivery.delivery_id).await?;
                report.delivered = report.delivered.saturating_add(1);
            }
            SinkOutcome::Rejected { reason } => {
                // It will keep failing (e.g. `410 Gone`): bury now.
                self.queue.bury(&attempted, &reason, now).await?;
                report.buried = report.buried.saturating_add(1);
            }
            SinkOutcome::Retry { reason } => {
                let failing_since = attempted.first_failed_at.unwrap_or(now);
                if self.policy.is_exhausted(failing_since, now) {
                    self.queue.bury(&attempted, &reason, now).await?;
                    report.buried = report.buried.saturating_add(1);
                } else {
                    let next = now.saturating_add(to_time(self.policy.backoff(attempted.attempts)));
                    self.queue
                        .enqueue(
                            &SinkDelivery {
                                first_failed_at: Some(failing_since),
                                ..attempted
                            },
                            next,
                        )
                        .await?;
                    report.retried = report.retried.saturating_add(1);
                }
                return Ok(Endpoint::Failing);
            }
        }

        Ok(Endpoint::Answering)
    }

    /// A delivery whose sink this replica's configuration does not declare.
    ///
    /// Never completed — see [`DrainSinks`] for why "unknown here" is not "gone".
    /// No attempt is counted, because none was made; the give-up clock does start,
    /// so a sink that really left the file reaches the dead-letter queue after the
    /// same window as one whose endpoint stayed down.
    async fn defer_orphan(
        &self,
        delivery: &SinkDelivery,
        report: &mut DrainReport,
    ) -> Result<(), Failure> {
        let now = self.clock.now();
        let since = delivery.first_failed_at.unwrap_or(now);

        if self.policy.is_exhausted(since, now) {
            tracing::warn!(
                delivery = %delivery.delivery_id,
                channel = %delivery.channel,
                sink = %delivery.sink,
                "burying a delivery whose sink no replica that led has declared for a day"
            );
            self.queue
                .bury(
                    delivery,
                    "sink not configured on the replica that drained it for the whole \
                     retry window; removed from the configuration?",
                    now,
                )
                .await?;
            report.buried = report.buried.saturating_add(1);
        } else {
            tracing::info!(
                delivery = %delivery.delivery_id,
                channel = %delivery.channel,
                sink = %delivery.sink,
                "sink not configured on this replica; leaving the delivery for one that knows it"
            );
            self.queue
                .enqueue(
                    &SinkDelivery {
                        first_failed_at: Some(since),
                        ..delivery.clone()
                    },
                    now.saturating_add(ORPHAN_RECHECK),
                )
                .await?;
            report.orphaned = report.orphaned.saturating_add(1);
        }
        Ok(())
    }
}

/// Whether the endpoint behind a delivery is worth another attempt this pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Endpoint {
    Answering,
    Failing,
}

/// A backoff as the clock's duration type. Saturates rather than failing: a wait
/// too long to represent is "not this pass", which zero would get wrong.
fn to_time(wait: core::time::Duration) -> time::Duration {
    time::Duration::try_from(wait).unwrap_or(time::Duration::MAX)
}

/// `GET /api/v1/admin/dlq` — what is stuck (design §8, §10.4bis).
#[derive(Clone)]
pub struct ListDeadLetters {
    queue: Arc<dyn SinkQueue>,
}

impl core::fmt::Debug for ListDeadLetters {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ListDeadLetters").finish_non_exhaustive()
    }
}

impl ListDeadLetters {
    #[must_use]
    pub fn new(queue: Arc<dyn SinkQueue>) -> Self {
        Self { queue }
    }

    /// Every dead letter, or one channel's. Inside the §5.4 boundary: a dead letter
    /// names channel, sink and error, never the event content.
    pub async fn execute(
        &self,
        caller: &Caller,
        channel: Option<&ChannelId>,
    ) -> Result<Vec<DeadLetter>, Failure> {
        caller.ensure_platform_admin()?;
        Ok(self.queue.dead_letters(channel).await?)
    }
}

/// `POST /api/v1/admin/dlq/{id}/requeue` — try again (design §8, scenario T17).
#[derive(Clone)]
pub struct RequeueDeadLetter {
    queue: Arc<dyn SinkQueue>,
    audit: Arc<dyn AuditLog>,
    clock: Arc<dyn Clock>,
}

impl core::fmt::Debug for RequeueDeadLetter {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RequeueDeadLetter").finish_non_exhaustive()
    }
}

impl RequeueDeadLetter {
    #[must_use]
    pub fn new(queue: Arc<dyn SinkQueue>, audit: Arc<dyn AuditLog>, clock: Arc<dyn Clock>) -> Self {
        Self {
            queue,
            audit,
            clock,
        }
    }

    /// Put one back in the queue and audit it (§5.4); answers whether there was one.
    pub async fn execute(&self, caller: &Caller, id: &DeliveryId) -> Result<bool, Failure> {
        caller.ensure_platform_admin()?;

        let now = self.clock.now();
        if !self.queue.requeue(id, now).await? {
            // No entry, no audit line.
            return Ok(false);
        }

        self.audit
            .record(&AuditEntry {
                actor: caller.principal.clone().into(),
                action: actions::DLQ_REQUEUED,
                target: Some(id.to_string()),
                channel: None,
                detail: None,
                at: now,
            })
            .await?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use notify_domain::{Principal, UserId};
    use notify_ports::SinkTarget;
    use notify_ports::test_utils::{
        InMemory, RecordingSinkDispatcher, StaticSinkRegistry, TestClock, TestLeaderElection,
    };
    use time::OffsetDateTime;
    use url::Url;

    use super::*;

    fn channel_id() -> ChannelId {
        ChannelId::parse("ci-infra").unwrap()
    }

    fn webhook() -> SinkTarget {
        SinkTarget::Webhook {
            url: Url::parse("https://hooks.weebo.poc/incidents").unwrap(),
            secret: "shh".to_owned(),
        }
    }

    struct Fixture {
        store: InMemory,
        clock: TestClock,
        dispatcher: Arc<RecordingSinkDispatcher>,
        registry: StaticSinkRegistry,
        drain: DrainSinks,
    }

    fn fixture() -> Fixture {
        let store = InMemory::new();
        let clock = TestClock::new();
        let dispatcher = Arc::new(RecordingSinkDispatcher::new());
        let registry = StaticSinkRegistry::empty().route(channel_id(), webhook());

        let drain = DrainSinks::new(
            Arc::new(store.clone()),
            Arc::new(registry.clone()),
            dispatcher.clone(),
            Arc::new(TestLeaderElection::leader()),
            Arc::new(clock.clone()),
        );

        Fixture {
            store,
            clock,
            dispatcher,
            registry,
            drain,
        }
    }

    /// The event a publish would have queued.
    fn event(at: OffsetDateTime) -> notify_ports::Event {
        let alice = Principal::User {
            id: UserId::generate(),
        };
        let (notification, written) = notify_domain::Notification::create(
            notify_domain::NotificationId::generate(),
            channel_id(),
            None,
            notify_domain::RevisionDraft::lifecycle(
                notify_domain::State::Open,
                notify_domain::ContentDraft::titled("etcd degraded"),
            ),
            notify_domain::Author::anonymous(alice),
            at,
            &notify_domain::ChannelPolicy::default(),
        )
        .unwrap();

        notify_ports::Event {
            cursor: notify_domain::Cursor::from_sequence(1),
            audience: notify_ports::Audience::Channel { id: channel_id() },
            recorded_at: at,
            muted_by_dnd: false,
            payload: notify_ports::EventPayload::Revision(Box::new(
                notify_ports::RevisionEvent::new(&notification, &written.revision),
            )),
        }
    }

    /// One queued delivery on `ci-infra`.
    async fn queue_one(fixture: &Fixture) -> SinkDelivery {
        let event = event(fixture.clock.now());
        let delivery = SinkDelivery {
            delivery_id: DeliveryId::generate(),
            channel: channel_id(),
            sink: webhook()
                .id(&notify_ports::test_utils::test_fingerprint_key())
                .unwrap(),
            event,
            attempts: 0,
            first_failed_at: None,
        };
        SinkQueue::enqueue(&fixture.store, &delivery, fixture.clock.now())
            .await
            .unwrap();
        delivery
    }

    #[tokio::test]
    async fn a_queued_delivery_is_sent_with_the_target_resolved_from_the_configuration() {
        let fixture = fixture();
        queue_one(&fixture).await;

        let report = fixture.drain.execute().await.unwrap();

        assert_eq!(report.delivered, 1);
        assert_eq!(
            fixture.dispatcher.targets(),
            vec![webhook()],
            "the secret comes from the registry at send time, never from the queue row"
        );
        assert_eq!(
            fixture.dispatcher.delivered()[0].attempts,
            1,
            "the dispatcher is told which attempt this is"
        );
        assert!(
            SinkQueue::claim_due(&fixture.store, fixture.clock.now(), 10, fixture.clock.now())
                .await
                .unwrap()
                .is_empty(),
            "an accepted delivery is done with"
        );
    }

    #[tokio::test]
    async fn a_replica_that_does_not_lead_sends_nothing() {
        let fixture = fixture();
        queue_one(&fixture).await;

        let drain = DrainSinks::new(
            Arc::new(fixture.store.clone()),
            Arc::new(fixture.registry.clone()),
            fixture.dispatcher.clone(),
            Arc::new(TestLeaderElection::follower()),
            Arc::new(fixture.clock.clone()),
        );

        let report = drain.execute().await.unwrap();
        assert!(!report.led);
        assert!(
            fixture.dispatcher.delivered().is_empty(),
            "every replica draining would deliver the same webhook once each (§11)"
        );
    }

    #[tokio::test]
    async fn a_failure_is_rescheduled_with_the_published_backoff() {
        let fixture = fixture();
        queue_one(&fixture).await;
        fixture.dispatcher.always(SinkOutcome::Retry {
            reason: "connection refused".to_owned(),
        });

        assert_eq!(fixture.drain.execute().await.unwrap().retried, 1);

        // Not due yet: the first retry waits the published thirty seconds.
        let soon = fixture.clock.now() + time::Duration::seconds(29);
        assert_eq!(
            SinkQueue::claim_due(&fixture.store, soon, 10, soon)
                .await
                .unwrap(),
            [] as [notify_ports::SinkDelivery; 0]
        );

        let later = fixture.clock.now() + time::Duration::seconds(30);
        let due = SinkQueue::claim_due(&fixture.store, later, 10, later)
            .await
            .unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].attempts, 1, "the attempt count carries over");
        assert!(
            due[0].first_failed_at.is_some(),
            "the give-up clock starts at the first failure, not at the publish"
        );
    }

    #[tokio::test]
    async fn a_refusal_that_will_not_change_goes_straight_to_the_dead_letter_queue() {
        let fixture = fixture();
        queue_one(&fixture).await;
        fixture.dispatcher.always(SinkOutcome::Rejected {
            reason: "410 Gone".to_owned(),
        });

        assert_eq!(fixture.drain.execute().await.unwrap().buried, 1);

        let letters = SinkQueue::dead_letters(&fixture.store, None).await.unwrap();
        assert_eq!(letters.len(), 1);
        assert_eq!(letters[0].last_error, "410 Gone");
        assert!(
            letters[0]
                .sink
                .destination()
                .starts_with("https://hooks.weebo.poc#"),
            "where to go and look, without the url's path: {}",
            letters[0].sink
        );
    }

    #[tokio::test]
    async fn a_delivery_whose_sink_this_replica_does_not_know_is_kept_not_dropped() {
        // A rolling deploy: the replica that leads has an older file. The delivery
        // belongs to whichever replica *does* know the sink, so it stays owed.
        let fixture = fixture();
        let delivery = queue_one(&fixture).await;
        fixture.registry.withdraw(&delivery.sink);

        let report = fixture.drain.execute().await.unwrap();

        assert_eq!(report.orphaned, 1);
        assert_eq!(
            fixture.dispatcher.delivered(),
            [] as [notify_ports::SinkDelivery; 0]
        );
        assert!(
            SinkQueue::dead_letters(&fixture.store, None)
                .await
                .unwrap()
                .is_empty(),
            "not buried on the first sight of an unknown sink"
        );

        // Still queued, and back once the recheck delay is up — where a replica
        // that knows the sink sends it.
        let later = fixture.clock.now() + ORPHAN_RECHECK;
        let due = SinkQueue::claim_due(&fixture.store, later, 10, later)
            .await
            .unwrap();
        assert_eq!(
            due.len(),
            1,
            "an unknown sink must never complete a delivery"
        );
        assert_eq!(due[0].attempts, 0, "no attempt was made");
        assert!(
            due[0].first_failed_at.is_some(),
            "the give-up clock started"
        );
    }

    #[tokio::test]
    async fn a_sink_unknown_for_the_whole_retry_window_ends_in_the_dead_letter_queue() {
        let fixture = fixture();
        let delivery = queue_one(&fixture).await;
        fixture.registry.withdraw(&delivery.sink);
        SinkQueue::enqueue(
            &fixture.store,
            &SinkDelivery {
                first_failed_at: Some(fixture.clock.now() - time::Duration::hours(25)),
                ..delivery
            },
            fixture.clock.now(),
        )
        .await
        .unwrap();

        let report = fixture.drain.execute().await.unwrap();

        assert_eq!(report.buried, 1);
        let letters = SinkQueue::dead_letters(&fixture.store, None).await.unwrap();
        assert!(
            letters[0].last_error.contains("not configured"),
            "the reason says why: {}",
            letters[0].last_error
        );
    }

    #[tokio::test]
    async fn a_failing_endpoint_stops_its_lane_and_releases_the_rest() {
        // Three deliveries to one sink and an endpoint that times out: trying all
        // three would spend three timeouts on the same answer.
        let fixture = fixture();
        queue_one(&fixture).await;
        queue_one(&fixture).await;
        queue_one(&fixture).await;
        fixture.dispatcher.always(SinkOutcome::Retry {
            reason: "timed out".to_owned(),
        });

        let report = fixture.drain.execute().await.unwrap();

        assert_eq!(report.retried, 1);
        assert_eq!(
            fixture.dispatcher.delivered().len(),
            1,
            "one attempt, not three"
        );
        let later = fixture.clock.now() + time::Duration::seconds(30);
        assert_eq!(
            SinkQueue::claim_due(&fixture.store, later, 10, later)
                .await
                .unwrap()
                .len(),
            3,
            "nothing is lost: the other two are owed again after the first backoff"
        );
    }

    #[tokio::test]
    async fn old_dead_letters_are_forgotten_after_the_retention() {
        let fixture = fixture();
        queue_one(&fixture).await;
        fixture.dispatcher.always(SinkOutcome::Rejected {
            reason: "410 Gone".to_owned(),
        });
        let drain = fixture
            .drain
            .clone()
            .with_dead_letter_retention(time::Duration::days(30));
        drain.execute().await.unwrap();
        assert_eq!(
            SinkQueue::dead_letters(&fixture.store, None)
                .await
                .unwrap()
                .len(),
            1
        );

        fixture
            .clock
            .set(fixture.clock.now() + time::Duration::days(31));
        assert_eq!(drain.execute().await.unwrap().pruned, 1);
        assert_eq!(
            SinkQueue::dead_letters(&fixture.store, None).await.unwrap(),
            [] as [notify_ports::DeadLetter; 0]
        );
    }

    #[tokio::test]
    async fn a_day_of_failing_ends_in_the_dead_letter_queue() {
        // An endpoint that stays down is not retried for ever (T17).
        let fixture = fixture();
        let delivery = queue_one(&fixture).await;
        fixture.dispatcher.always(SinkOutcome::Retry {
            reason: "connection refused".to_owned(),
        });

        // Already failing since yesterday.
        SinkQueue::enqueue(
            &fixture.store,
            &SinkDelivery {
                attempts: 12,
                first_failed_at: Some(fixture.clock.now() - time::Duration::hours(25)),
                ..delivery
            },
            fixture.clock.now(),
        )
        .await
        .unwrap();

        let report = fixture.drain.execute().await.unwrap();

        assert_eq!(report.buried, 1);
        assert_eq!(report.retried, 0);
        let letters = SinkQueue::dead_letters(&fixture.store, None).await.unwrap();
        assert_eq!(letters[0].attempts, 13);
    }

    /// T17 end to end: a down endpoint is retried on schedule, abandoned after a day,
    /// then requeued by hand and delivered (§3.4, §7.1bis).
    #[tokio::test]
    async fn critical_t17_an_endpoint_that_is_down_backs_off_dies_and_comes_back() {
        let fixture = fixture();
        let delivery = queue_one(&fixture).await;
        fixture.dispatcher.always(SinkOutcome::Retry {
            reason: "connection refused".to_owned(),
        });

        // Two failures, and the wait doubles between them: 30 s, then 60 s.
        let mut clock = fixture.clock.now();
        for expected in [30_i64, 60] {
            assert_eq!(fixture.drain.execute().await.unwrap().retried, 1);

            let just_before = clock + time::Duration::seconds(expected - 1);
            assert!(
                SinkQueue::claim_due(&fixture.store, just_before, 10, just_before)
                    .await
                    .unwrap()
                    .is_empty(),
                "the {expected}s backoff is not up yet"
            );

            clock += time::Duration::seconds(expected);
            fixture.clock.set(clock);
        }

        // A day after the first failure it stops being worth trying.
        fixture.clock.set(clock + time::Duration::hours(24));
        let report = fixture.drain.execute().await.unwrap();
        assert_eq!(report.buried, 1);
        assert_eq!(report.retried, 0);

        let letters = SinkQueue::dead_letters(&fixture.store, None).await.unwrap();
        assert_eq!(letters.len(), 1);
        assert_eq!(letters[0].attempts, 3, "three attempts, then given up on");

        // An operator fixes the endpoint and presses the button.
        let requeue = RequeueDeadLetter::new(
            Arc::new(fixture.store.clone()),
            Arc::new(fixture.store.clone()),
            Arc::new(fixture.clock.clone()),
        );
        assert!(
            requeue
                .execute(&admin(), &delivery.delivery_id)
                .await
                .unwrap()
        );

        fixture.dispatcher.always(SinkOutcome::Delivered);
        let report = fixture.drain.execute().await.unwrap();

        assert_eq!(report.delivered, 1, "the requeued delivery is sendable");
        assert_eq!(
            SinkQueue::dead_letters(&fixture.store, None).await.unwrap(),
            [] as [notify_ports::DeadLetter; 0]
        );
        assert_eq!(
            fixture.dispatcher.delivered().last().unwrap().delivery_id,
            delivery.delivery_id,
            "the consumer's idempotency key survives every retry and the requeue"
        );
    }

    // --- The two DLQ operations -------------------------------------------

    fn admin() -> Caller {
        Caller {
            platform_admin: true,
            ..Caller::member_of(
                Principal::User {
                    id: UserId::generate(),
                },
                vec!["platform-admins".to_owned()],
            )
        }
    }

    #[tokio::test]
    async fn a_requeue_puts_it_back_and_says_who_asked() {
        let fixture = fixture();
        let delivery = queue_one(&fixture).await;
        fixture.dispatcher.always(SinkOutcome::Rejected {
            reason: "410 Gone".to_owned(),
        });
        fixture.drain.execute().await.unwrap();

        let requeue = RequeueDeadLetter::new(
            Arc::new(fixture.store.clone()),
            Arc::new(fixture.store.clone()),
            Arc::new(fixture.clock.clone()),
        );

        assert!(
            requeue
                .execute(&admin(), &delivery.delivery_id)
                .await
                .unwrap()
        );
        assert!(
            !requeue
                .execute(&admin(), &delivery.delivery_id)
                .await
                .unwrap(),
            "the second one has nothing to put back"
        );

        let audited = AuditLog::query(
            &fixture.store,
            &notify_ports::AuditQuery {
                action: Some("dlq.requeued".to_owned()),
                ..notify_ports::AuditQuery::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            audited.items.len(),
            1,
            "one requeue happened, so one line is recorded — not two"
        );
    }

    #[tokio::test]
    async fn the_dlq_is_platform_role_only() {
        let fixture = fixture();
        let caller = Caller::member(Principal::User {
            id: UserId::generate(),
        });

        let list = ListDeadLetters::new(Arc::new(fixture.store.clone()));
        assert!(list.execute(&caller, None).await.is_err());

        let requeue = RequeueDeadLetter::new(
            Arc::new(fixture.store.clone()),
            Arc::new(fixture.store.clone()),
            Arc::new(fixture.clock.clone()),
        );
        assert!(
            requeue
                .execute(&caller, &DeliveryId::generate())
                .await
                .is_err()
        );
    }
}
