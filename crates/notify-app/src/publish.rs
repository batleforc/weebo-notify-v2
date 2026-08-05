//! Writing notifications: the two ingestion paths of design §8.3.
//!
//! `POST /notifications` creates; `PUT /channels/{id}/notifications/{external_id}`
//! upserts. Everything else that publishes — the CLI, the TUI's `:` mode, `curl`,
//! a CI job — is a wrapper around one of these two. There is no third ingestion
//! protocol, which is what makes quotas, hygiene and attribution enforceable in one
//! place.
//!
//! # Idempotency, twice over
//!
//! The upsert is idempotent **by construction**: the external id is the key, so a
//! retried CI job produces one notification and two revisions. `POST` has no such
//! handle, so it takes an `Idempotency-Key` and replays the stored response for 24
//! hours (scenario T16).
//!
//! The upsert also has to survive *concurrency*, not just retries: two parallel
//! jobs racing on the same external id both find nothing, both insert, and one
//! loses. Losing is expected — the loser re-reads and revises, which is the same
//! outcome it would have had a millisecond later.

use std::sync::Arc;

use notify_domain::{
    ChannelId, Cursor, DomainError, ExternalId, Notification, NotificationId, Revision,
    RevisionDraft, Scope, TtlResolution,
};
use notify_ports::{
    Allowance, Claim, Clock, Failure, IdempotencyStore, NotificationRepository, PortError, Quota,
    RateLimiter,
};
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::authz::{Authorizer, Caller};
use crate::fanout::Fanout;

/// How long a replayed `Idempotency-Key` response stays available (design §8).
const IDEMPOTENCY_WINDOW: Duration = Duration::hours(24);

/// What a publish produced.
///
/// Serialisable because it *is* the stored idempotency response: replaying a key
/// has to give back the same answer, and the only way to be sure of that is to
/// return the same value rather than recompute it.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Published {
    /// The notification as it now stands.
    pub notification: Notification,
    /// The revision that was just recorded.
    pub revision: Revision,
    /// What the channel policy did to the requested retention — a clamp the
    /// publisher cannot see is a clamp they will report as a bug (scenario T6).
    pub retention: TtlResolution,
    /// Where it landed in the event log.
    pub cursor: Cursor,
    /// Whether this is a replay of an earlier identical request.
    pub replayed: bool,
}

/// The collaborators both publishing use cases need.
#[derive(Clone)]
struct Writer {
    notifications: Arc<dyn NotificationRepository>,
    idempotency: Arc<dyn IdempotencyStore>,
    limiter: Arc<dyn RateLimiter>,
    clock: Arc<dyn Clock>,
    authorizer: Authorizer,
    fanout: Fanout,
}

impl core::fmt::Debug for Writer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Writer").finish_non_exhaustive()
    }
}

impl Writer {
    /// Check both publish quotas: the key's own budget and the channel's.
    ///
    /// Two limits rather than one because they protect different things — a
    /// runaway publisher should degrade itself, and a channel should survive
    /// several well-behaved ones (design §8.2, scenario T14).
    async fn check_quotas(
        &self,
        caller: &Caller,
        channel: &ChannelId,
        now: OffsetDateTime,
    ) -> Result<(), Failure> {
        let per_key = Quota::PublishPerKey {
            key: caller.principal.reference(),
        };
        let per_channel = Quota::PublishPerChannel {
            channel: channel.to_string(),
        };

        for quota in [per_key, per_channel] {
            if let Allowance::Limited {
                retry_after_seconds,
            } = self.limiter.check(&quota, now).await?
            {
                return Err(Failure::RateLimited {
                    retry_after_seconds,
                });
            }
        }
        Ok(())
    }

    /// Record a first revision, then fan it out.
    async fn create(
        &self,
        caller: &Caller,
        channel: &notify_domain::Channel,
        external_id: Option<ExternalId>,
        draft: RevisionDraft,
        now: OffsetDateTime,
    ) -> Result<Published, Failure> {
        let author = notify_domain::Author::anonymous(caller.principal.clone());
        let (notification, written) = Notification::create(
            NotificationId::generate(),
            channel.id.clone(),
            external_id,
            draft,
            author,
            now,
            &channel.policy,
        )?;

        let cursor = self
            .notifications
            .insert(&notification, &written.revision)
            .await?;
        self.after_write(&notification, cursor).await;

        Ok(Published {
            notification,
            revision: written.revision,
            retention: written.retention,
            cursor,
            replayed: false,
        })
    }

    /// Append a revision to an existing notification, then fan it out.
    async fn revise(
        &self,
        caller: &Caller,
        channel: &notify_domain::Channel,
        mut notification: Notification,
        draft: RevisionDraft,
        now: OffsetDateTime,
    ) -> Result<Published, Failure> {
        let author = notify_domain::Author::anonymous(caller.principal.clone());
        let written = notification.revise(draft, author, now, &channel.policy)?;

        let cursor = self
            .notifications
            .append_revision(&notification, &written.revision)
            .await?;
        self.after_write(&notification, cursor).await;

        Ok(Published {
            notification,
            revision: written.revision,
            retention: written.retention,
            cursor,
            replayed: false,
        })
    }

    async fn after_write(&self, notification: &Notification, cursor: Cursor) {
        self.fanout.poke_channel(notification, cursor).await;
        self.fanout.push(notification).await;
    }
}

/// `POST /api/v1/notifications` — create a notification (design §8).
#[derive(Debug, Clone)]
pub struct PublishNotification {
    writer: Writer,
}

/// What a caller asks for.
#[derive(Debug, Clone)]
pub struct PublishRequest {
    /// Where it goes.
    pub channel: ChannelId,
    /// What to publish.
    pub draft: RevisionDraft,
    /// The `Idempotency-Key` header, when the caller sent one.
    pub idempotency_key: Option<String>,
}

impl PublishNotification {
    /// Wire it.
    #[must_use]
    pub fn new(
        notifications: Arc<dyn NotificationRepository>,
        idempotency: Arc<dyn IdempotencyStore>,
        limiter: Arc<dyn RateLimiter>,
        clock: Arc<dyn Clock>,
        authorizer: Authorizer,
        fanout: Fanout,
    ) -> Self {
        Self {
            writer: Writer {
                notifications,
                idempotency,
                limiter,
                clock,
                authorizer,
                fanout,
            },
        }
    }

    /// Publish.
    pub async fn execute(
        &self,
        caller: &Caller,
        request: PublishRequest,
    ) -> Result<Published, Failure> {
        let now = self.writer.clock.now();
        let channel = self
            .writer
            .authorizer
            .authorize(caller, &request.channel, Scope::Publish)
            .await?;
        self.writer
            .check_quotas(caller, &request.channel, now)
            .await?;

        let Some(key) = request.idempotency_key.clone() else {
            return self
                .writer
                .create(caller, &channel, None, request.draft, now)
                .await;
        };

        // Claim before doing the work: a retry arriving mid-flight has to wait
        // rather than start a second publish.
        let principal_ref = caller.principal.reference();
        match self
            .writer
            .idempotency
            .claim(&principal_ref, &key, now)
            .await?
        {
            Claim::Replayed { response } => {
                let mut replayed: Published = serde_json::from_str(&response)
                    .map_err(|error| PortError::corrupt("idempotency.response", error))?;
                replayed.replayed = true;
                Ok(replayed)
            }
            Claim::InFlight => Err(Failure::Port(PortError::Conflict {
                what: "idempotency_key",
            })),
            Claim::Fresh => {
                let published = match self
                    .writer
                    .create(caller, &channel, None, request.draft, now)
                    .await
                {
                    Ok(published) => published,
                    Err(failure) => {
                        // Leaving the claim in place would answer the client's
                        // retry with `InFlight` for the next 24 hours.
                        let _ = self.writer.idempotency.release(&principal_ref, &key).await;
                        return Err(failure);
                    }
                };

                let encoded = serde_json::to_string(&published)
                    .map_err(|error| PortError::corrupt("idempotency.response", error))?;
                self.writer
                    .idempotency
                    .complete(&principal_ref, &key, &encoded, now + IDEMPOTENCY_WINDOW)
                    .await?;
                Ok(published)
            }
        }
    }
}

/// `PUT /api/v1/channels/{id}/notifications/{external_id}` — the idempotent upsert
/// CI pipelines are built on (design §2.2, §8).
#[derive(Debug, Clone)]
pub struct UpsertNotification {
    writer: Writer,
}

/// What a caller asks for.
#[derive(Debug, Clone)]
pub struct UpsertRequest {
    /// Where it goes.
    pub channel: ChannelId,
    /// The publisher's own identifier.
    pub external_id: ExternalId,
    /// What to publish.
    pub draft: RevisionDraft,
}

impl UpsertNotification {
    /// Wire it.
    #[must_use]
    pub fn new(
        notifications: Arc<dyn NotificationRepository>,
        idempotency: Arc<dyn IdempotencyStore>,
        limiter: Arc<dyn RateLimiter>,
        clock: Arc<dyn Clock>,
        authorizer: Authorizer,
        fanout: Fanout,
    ) -> Self {
        Self {
            writer: Writer {
                notifications,
                idempotency,
                limiter,
                clock,
                authorizer,
                fanout,
            },
        }
    }

    /// Create or revise.
    pub async fn execute(
        &self,
        caller: &Caller,
        request: UpsertRequest,
    ) -> Result<Published, Failure> {
        let now = self.writer.clock.now();
        let channel = self
            .writer
            .authorizer
            .authorize(caller, &request.channel, Scope::Publish)
            .await?;
        self.writer
            .check_quotas(caller, &request.channel, now)
            .await?;

        let existing = self
            .writer
            .notifications
            .find_by_external_id(&request.channel, &request.external_id)
            .await?;

        if let Some(notification) = existing {
            return self
                .writer
                .revise(caller, &channel, notification, request.draft, now)
                .await;
        }

        match self
            .writer
            .create(
                caller,
                &channel,
                Some(request.external_id.clone()),
                request.draft.clone(),
                now,
            )
            .await
        {
            Ok(published) => Ok(published),
            // A parallel job won the race. Losing it is not an error: re-read and
            // revise, which is the outcome we would have had a moment later.
            Err(Failure::Port(PortError::Conflict { .. })) => {
                let notification = self
                    .writer
                    .notifications
                    .find_by_external_id(&request.channel, &request.external_id)
                    .await?
                    .ok_or(DomainError::NotificationGone)?;
                self.writer
                    .revise(caller, &channel, notification, request.draft, now)
                    .await
            }
            Err(other) => Err(other),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use notify_domain::{
        Channel, ChannelPolicy, ContentDraft, Membership, Principal, State, Status, TtlPolicy,
        UserId,
    };
    use notify_ports::test_utils::{InMemory, TestClock, TestRateLimiter};

    use super::*;
    use crate::authz::Grant;

    struct Fixture {
        store: InMemory,
        clock: TestClock,
        limiter: TestRateLimiter,
        publish: PublishNotification,
        upsert: UpsertNotification,
        alice: Principal,
    }

    fn channel_id() -> ChannelId {
        ChannelId::parse("ci-infra").unwrap()
    }

    fn fixture(policy: ChannelPolicy) -> Fixture {
        let store = InMemory::new();
        let clock = TestClock::new();
        let limiter = TestRateLimiter::new();
        let alice = Principal::User {
            id: UserId::generate(),
        };

        let mut channel = Channel::dynamic(channel_id(), None).unwrap();
        channel.policy = policy;
        channel.upsert_member(
            Membership::new(alice.clone(), vec![Scope::Publish, Scope::Subscribe]).unwrap(),
        );
        store.seed_channel(channel);

        let authorizer = Authorizer::new(Arc::new(store.clone()));
        let fanout = Fanout::new(
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(clock.clone()),
            4_096,
        );

        Fixture {
            publish: PublishNotification::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(limiter.clone()),
                Arc::new(clock.clone()),
                authorizer.clone(),
                fanout.clone(),
            ),
            upsert: UpsertNotification::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(limiter.clone()),
                Arc::new(clock.clone()),
                authorizer,
                fanout,
            ),
            store,
            clock,
            limiter,
            alice,
        }
    }

    fn request(key: Option<&str>) -> PublishRequest {
        PublishRequest {
            channel: channel_id(),
            draft: RevisionDraft::simple(ContentDraft::titled("deploy prod")),
            idempotency_key: key.map(str::to_owned),
        }
    }

    #[tokio::test]
    async fn a_publish_records_a_revision_and_pokes_the_bus() {
        let fixture = fixture(ChannelPolicy::default());
        let caller = Caller::member(fixture.alice.clone());

        let published = fixture
            .publish
            .execute(&caller, request(None))
            .await
            .unwrap();

        assert_eq!(published.revision.seq, 1);
        assert!(!published.replayed);
        assert_eq!(published.notification.status(), Status::Info);
        assert_eq!(fixture.store.pokes().len(), 1);
    }

    #[tokio::test]
    async fn publishing_needs_the_publish_scope() {
        let fixture = fixture(ChannelPolicy::default());
        let stranger = Caller::member(Principal::User {
            id: UserId::generate(),
        });

        assert_eq!(
            fixture
                .publish
                .execute(&stranger, request(None))
                .await
                .unwrap_err()
                .code(),
            Some("scope.missing")
        );
    }

    // --- T16 · Idempotency (design §3.4) ----------------------------------

    #[tokio::test]
    async fn critical_t16_a_replayed_idempotency_key_creates_one_notification() {
        let fixture = fixture(ChannelPolicy::default());
        let caller = Caller::member(fixture.alice.clone());

        let first = fixture
            .publish
            .execute(&caller, request(Some("deploy-1")))
            .await
            .unwrap();
        let second = fixture
            .publish
            .execute(&caller, request(Some("deploy-1")))
            .await
            .unwrap();

        assert!(second.replayed);
        assert_eq!(first.notification.id, second.notification.id);
        assert_eq!(first.cursor, second.cursor);
        assert_eq!(
            fixture.store.pokes().len(),
            1,
            "a replay must not fan out a second time"
        );
    }

    #[tokio::test]
    async fn critical_t16_a_failed_publish_frees_its_idempotency_key() {
        let fixture = fixture(ChannelPolicy::default());
        let caller = Caller::member(fixture.alice.clone());

        // A payload the domain refuses.
        let bad = PublishRequest {
            draft: RevisionDraft::simple(ContentDraft::titled("   ")),
            ..request(Some("deploy-1"))
        };
        assert!(fixture.publish.execute(&caller, bad).await.is_err());

        // The client fixes it and retries with the same key.
        let recovered = fixture
            .publish
            .execute(&caller, request(Some("deploy-1")))
            .await
            .unwrap();
        assert!(!recovered.replayed);
    }

    #[tokio::test]
    async fn critical_t16_an_upsert_makes_one_notification_and_two_revisions() {
        let fixture = fixture(ChannelPolicy::default());
        let caller = Caller::member(fixture.alice.clone());
        let external = ExternalId::parse("run-4521").unwrap();

        let started = fixture
            .upsert
            .execute(
                &caller,
                UpsertRequest {
                    channel: channel_id(),
                    external_id: external.clone(),
                    draft: RevisionDraft::lifecycle(
                        State::Open,
                        ContentDraft::titled("build started"),
                    ),
                },
            )
            .await
            .unwrap();

        let finished = fixture
            .upsert
            .execute(
                &caller,
                UpsertRequest {
                    channel: channel_id(),
                    external_id: external,
                    draft: RevisionDraft {
                        state: Some(State::Closed),
                        status: Some(Status::Success),
                        ..RevisionDraft::simple(ContentDraft::titled("build finished"))
                    },
                },
            )
            .await
            .unwrap();

        assert_eq!(started.notification.id, finished.notification.id);
        assert_eq!(finished.revision.seq, 2);
        assert_eq!(finished.notification.status(), Status::Success);
    }

    // --- T6 · The clamp is visible ----------------------------------------

    #[tokio::test]
    async fn critical_t6_a_clamped_ttl_comes_back_in_the_response() {
        let fixture = fixture(ChannelPolicy {
            ttl: TtlPolicy {
                max: Some(notify_domain::HumanDuration::from_secs_const(3_600)),
                ..TtlPolicy::default()
            },
            ..ChannelPolicy::default()
        });
        let caller = Caller::member(fixture.alice.clone());

        let published = fixture
            .publish
            .execute(
                &caller,
                PublishRequest {
                    draft: RevisionDraft {
                        ttl: Some(notify_domain::Ttl::parse("30d", "ttl").unwrap()),
                        ..RevisionDraft::simple(ContentDraft::titled("kept briefly"))
                    },
                    ..request(None)
                },
            )
            .await
            .unwrap();

        assert!(published.retention.clamped_from.is_some());
    }

    // --- T14 · Rate limiting ----------------------------------------------

    #[tokio::test]
    async fn critical_t14_a_limited_publisher_is_refused_with_a_retry_after() {
        let fixture = fixture(ChannelPolicy::default());
        let caller = Caller::member(fixture.alice.clone());
        fixture.limiter.limit(Quota::PublishPerKey {
            key: fixture.alice.reference(),
        });

        let failure = fixture
            .publish
            .execute(&caller, request(None))
            .await
            .unwrap_err();

        assert!(matches!(
            failure,
            Failure::RateLimited {
                retry_after_seconds: 60
            }
        ));
        assert!(
            fixture.store.events().is_empty(),
            "the quota is consumed on the way in, before any work"
        );
    }

    #[tokio::test]
    async fn critical_t14_one_limited_publisher_does_not_affect_the_others() {
        let fixture = fixture(ChannelPolicy::default());
        let noisy = Caller {
            principal: fixture.alice.clone(),
            grant: Grant::ApiKey {
                scopes: vec![Scope::Publish],
                channels: vec![channel_id()],
            },
            groups: Vec::new(),
            platform_admin: false,
        };
        fixture.limiter.limit(Quota::PublishPerKey {
            key: fixture.alice.reference(),
        });

        assert!(
            fixture
                .publish
                .execute(&noisy, request(None))
                .await
                .is_err()
        );

        // A different principal in the same channel is untouched.
        let other = Principal::User {
            id: UserId::generate(),
        };
        let mut channel = Channel::dynamic(channel_id(), None).unwrap();
        channel.upsert_member(Membership::new(other.clone(), vec![Scope::Publish]).unwrap());
        channel
            .upsert_member(Membership::new(fixture.alice.clone(), vec![Scope::Publish]).unwrap());
        notify_ports::ChannelRepository::upsert(&fixture.store, &channel)
            .await
            .unwrap();

        assert!(
            fixture
                .publish
                .execute(&Caller::member(other), request(None))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn the_clock_is_the_one_the_use_case_was_wired_with() {
        let fixture = fixture(ChannelPolicy::default());
        let caller = Caller::member(fixture.alice.clone());
        fixture.clock.advance(Duration::days(7));

        let published = fixture
            .publish
            .execute(&caller, request(None))
            .await
            .unwrap();
        assert_eq!(published.revision.received_at, fixture.clock.now());
    }
}
