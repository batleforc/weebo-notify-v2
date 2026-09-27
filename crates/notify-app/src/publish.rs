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
    ChannelId, Cursor, ExternalId, Notification, NotificationId, Revision, RevisionDraft, Scope,
    TtlResolution,
};
use notify_ports::{
    Allowance, Claim, Clock, Failure, IdempotencyStore, NotificationRepository, PortError, Quota,
    RateLimiter,
};
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::attribution::Attribution;
use crate::authz::{Authorizer, Caller, Grant};
use crate::fanout::Fanout;

/// How long a replayed `Idempotency-Key` response stays available (design §8).
const IDEMPOTENCY_WINDOW: Duration = Duration::hours(24);

/// How many times an upsert re-reads after losing a race before giving up.
const UPSERT_ATTEMPTS: usize = 3;

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
    /// The name a client shows next to this revision (design §2.4, and the
    /// reasoning for naming here rather than on the read path is in
    /// [`crate::attribution`]).
    attribution: Attribution,
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
    ///
    /// Takes the loaded [`notify_domain::Channel`] rather than its id because the
    /// channel's own ceiling lives on its policy (§8.2, `limits` in §11). Both
    /// callers have already authorised, so the channel is in hand and this costs
    /// no extra read — which is why the override needed no reordering to honour.
    async fn check_quotas(
        &self,
        caller: &Caller,
        channel: &notify_domain::Channel,
        now: OffsetDateTime,
    ) -> Result<(), Failure> {
        // Per key when the caller is one, so a ceiling set on one key throttles that
        // key and not its owner's other keys (§8.2, D24); per principal otherwise.
        let per_key = Quota::PublishPerKey {
            key: match &caller.grant {
                Grant::ApiKey { id, .. } => format!("apikey:{id}"),
                Grant::Membership | Grant::Federated { .. } => caller.principal.reference(),
            },
        };
        let per_channel = Quota::PublishPerChannel {
            channel: channel.id.to_string(),
        };

        // A key's own ceiling, if a platform-admin set one (§8.2, D24) — carried on
        // the grant because that is where the key was already loaded. Membership
        // and federated callers have no key of their own to narrow, so `None`.
        let key_limit = match &caller.grant {
            Grant::ApiKey {
                publish_per_minute, ..
            } => *publish_per_minute,
            Grant::Membership | Grant::Federated { .. } => None,
        };

        for (quota, configured) in [
            (per_key, key_limit),
            (per_channel, channel.policy.publish_per_minute),
        ] {
            if let Allowance::Limited {
                retry_after_seconds,
            } = self.limiter.check(&quota, configured, now).await?
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
        let author = self.attribution.author(&caller.principal).await;
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
        self.after_write(&notification, &written.revision, cursor)
            .await;

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
        let author = self.attribution.author(&caller.principal).await;
        let written = notification.revise(draft, author, now, &channel.policy)?;

        let cursor = self
            .notifications
            .append_revision(&notification, &written.revision)
            .await?;
        self.after_write(&notification, &written.revision, cursor)
            .await;

        Ok(Published {
            notification,
            revision: written.revision,
            retention: written.retention,
            cursor,
            replayed: false,
        })
    }

    async fn after_write(
        &self,
        notification: &Notification,
        revision: &notify_domain::Revision,
        cursor: Cursor,
    ) {
        self.fanout.poke_channel(notification, cursor).await;
        self.fanout.push(notification).await;
        self.fanout
            .queue_sinks(notification, revision, cursor)
            .await;
        // Last: real-time delivery must not wait on a bucket fetch and a decode
        // for a thumbnail nobody has asked to see yet (design D30).
        self.fanout.derive_thumbnail(revision).await;
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
        attribution: Attribution,
    ) -> Self {
        Self {
            writer: Writer {
                notifications,
                idempotency,
                limiter,
                clock,
                authorizer,
                fanout,
                attribution,
            },
        }
    }

    /// Publish.
    ///
    /// The root of the trace §13 asks for (`publish → fanout → push transport`) when
    /// the caller sent no `traceparent`, and its continuation when they did. The
    /// span carries the channel and nothing from the draft: a trace is read by
    /// whoever runs the collector, and §13 says the logs never carry a payload —
    /// there is no reason the spans should.
    #[tracing::instrument(name = "publish", skip_all, fields(channel = %request.channel))]
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

        let Some(key) = request.idempotency_key.clone() else {
            self.writer.check_quotas(caller, &channel, now).await?;
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
                // The quota is charged only for work that will actually be done:
                // replaying a response costs nothing, so a retry of a publish that
                // already succeeded is never answered `429` instead of its replay.
                let published = match self.writer.check_quotas(caller, &channel, now).await {
                    Ok(()) => {
                        self.writer
                            .create(caller, &channel, None, request.draft, now)
                            .await
                    }
                    Err(failure) => Err(failure),
                };
                let published = match published {
                    Ok(published) => published,
                    Err(failure) => {
                        // Leaving the claim in place would answer the client's
                        // retry with `InFlight` until the claim lapses.
                        let _ = self
                            .writer
                            .idempotency
                            .release(&principal_ref, &key, now)
                            .await;
                        return Err(failure);
                    }
                };

                // The notification exists now, whatever happens next. Failing the
                // request because the replay could not be stored would send the
                // client to retry a publish that succeeded — and once the claim
                // lapses, that retry publishes twice. So the answer stands; only
                // the replay is lost.
                match serde_json::to_string(&published) {
                    Ok(encoded) => {
                        if let Err(error) = self
                            .writer
                            .idempotency
                            .complete(
                                &principal_ref,
                                &key,
                                now,
                                &encoded,
                                now + IDEMPOTENCY_WINDOW,
                            )
                            .await
                        {
                            tracing::warn!(%error, "published, but could not store the idempotent replay");
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "published, but could not encode the idempotent replay");
                    }
                }
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
        attribution: Attribution,
    ) -> Self {
        Self {
            writer: Writer {
                notifications,
                idempotency,
                limiter,
                clock,
                authorizer,
                fanout,
                attribution,
            },
        }
    }

    /// Create or revise.
    ///
    /// Same span as [`PublishNotification::execute`] in every respect but the name,
    /// and it carries the publisher's own id because that is the thing an operator
    /// has in hand when they come asking — "the run that failed was `build-4711`".
    #[tracing::instrument(
        name = "upsert",
        skip_all,
        fields(channel = %request.channel, external_id = %request.external_id)
    )]
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
        self.writer.check_quotas(caller, &channel, now).await?;

        // Every race this can lose — an insert beaten to the external id, a revision
        // beaten to the next seq, a notification deleted or expired between the read
        // and the write — is answered the same way: re-read and go again, which is
        // the outcome we would have had a moment later. Bounded, so an id under
        // constant contention fails instead of spinning.
        for _ in 0..UPSERT_ATTEMPTS {
            let existing = self
                .writer
                .notifications
                .find_by_external_id(&request.channel, &request.external_id)
                .await?;

            let outcome = match existing {
                Some(notification) => {
                    self.writer
                        .revise(caller, &channel, notification, request.draft.clone(), now)
                        .await
                }
                None => {
                    self.writer
                        .create(
                            caller,
                            &channel,
                            Some(request.external_id.clone()),
                            request.draft.clone(),
                            now,
                        )
                        .await
                }
            };

            match outcome {
                Err(Failure::Port(PortError::Conflict { .. })) => {}
                other => return other,
            }
        }

        Err(PortError::Conflict {
            what: "notification.external_id",
        }
        .into())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use notify_domain::{
        ApiKeyId, Channel, ChannelPolicy, ContentDraft, Membership, Principal, State, Status,
        TtlPolicy, UserId,
    };
    use notify_ports::test_utils::{InMemory, InMemoryAttachments, TestClock, TestRateLimiter};

    use super::*;

    /// Resolves or refuses every thumbnail the same way, so a test can assert the
    /// *wiring* in [`Writer::derive_thumbnail`] — a source that exists gets a
    /// thumbnail stored, a rejection or a missing source does not — without a
    /// real decoder. `notify-app` cannot depend on `notify-avatars` (an
    /// adapter), so this stands in for it the same way `InMemoryAttachments`
    /// stands in for `S3Attachments`.
    #[derive(Debug, Clone, Default)]
    struct StubThumbnailer {
        refuse: bool,
    }

    #[async_trait::async_trait]
    impl notify_ports::AttachmentThumbnailer for StubThumbnailer {
        async fn thumbnail(
            &self,
            bytes: &[u8],
        ) -> notify_ports::PortResult<
            Result<notify_ports::ProcessedImage, notify_ports::ImageRejection>,
        > {
            if self.refuse {
                return Ok(Err(notify_ports::ImageRejection::Unsupported));
            }
            Ok(Ok(notify_ports::ProcessedImage {
                content_type: "image/png".to_owned(),
                bytes: bytes.to_vec(),
            }))
        }
    }

    struct Fixture {
        store: InMemory,
        clock: TestClock,
        limiter: TestRateLimiter,
        publish: PublishNotification,
        upsert: UpsertNotification,
        alice: Principal,
        attachments: InMemoryAttachments,
    }

    fn channel_id() -> ChannelId {
        ChannelId::parse("ci-infra").unwrap()
    }

    fn fixture(policy: ChannelPolicy) -> Fixture {
        build(policy, false)
    }

    /// A fixture whose thumbnailer refuses every source, for the branch where a
    /// publish must succeed anyway (design D30).
    fn fixture_refusing_thumbnails(policy: ChannelPolicy) -> Fixture {
        build(policy, true)
    }

    fn build(policy: ChannelPolicy, refuse_thumbnails: bool) -> Fixture {
        let clock = TestClock::new();
        // One clock for the use case and the store, so a test that moves time
        // moves it for what the store considers expired too.
        let store = InMemory::with_clock(clock.clone());
        let limiter = TestRateLimiter::new();
        let attachments = InMemoryAttachments::new();
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
        let thumbnailer: Arc<dyn notify_ports::AttachmentThumbnailer> = Arc::new(StubThumbnailer {
            refuse: refuse_thumbnails,
        });
        let fanout = Fanout::new(
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(clock.clone()),
            4_096,
        )
        .with_thumbnails(Arc::new(attachments.clone()), thumbnailer);

        Fixture {
            publish: PublishNotification::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(limiter.clone()),
                Arc::new(clock.clone()),
                authorizer.clone(),
                fanout.clone(),
                Attribution::new(Arc::new(store.clone()), Arc::new(store.clone())),
            ),
            upsert: UpsertNotification::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(limiter.clone()),
                Arc::new(clock.clone()),
                authorizer,
                fanout,
                Attribution::new(Arc::new(store.clone()), Arc::new(store.clone())),
            ),
            store,
            clock,
            limiter,
            alice,
            attachments,
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
    async fn a_revision_carries_the_publisher_s_name_and_not_their_id() {
        // The property every inbox row and every timeline entry depended on and
        // nothing checked (§2.4). Falsify it by removing the `Attribution` call in
        // `Writer::create`: this asserts on the ULID instead, which is exactly what
        // the application rendered until this increment.
        let fixture = fixture(ChannelPolicy::default());
        let id = fixture.alice.as_user().unwrap();
        let user = notify_domain::User::from_claims(
            id,
            "alice-subject",
            None,
            Some("Alice Martin"),
            OffsetDateTime::now_utc(),
        )
        .unwrap();
        notify_ports::UserRepository::upsert(&fixture.store, &user)
            .await
            .unwrap();

        let caller = Caller::member(fixture.alice.clone());
        let published = fixture
            .publish
            .execute(&caller, request(None))
            .await
            .unwrap();

        assert_eq!(published.revision.author.label(), "Alice Martin");

        // And the second revision too: `revise` is a different code path, and it
        // was the one carrying half the bug.
        let revised = fixture
            .upsert
            .execute(
                &caller,
                UpsertRequest {
                    channel: channel_id(),
                    external_id: ExternalId::parse("build-4711").unwrap(),
                    draft: RevisionDraft::simple(ContentDraft::titled("deploy prod")),
                },
            )
            .await
            .unwrap();
        assert_eq!(revised.revision.author.label(), "Alice Martin");
        assert_ne!(published.revision.author.label(), id.to_string());
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

    #[tokio::test]
    async fn an_upsert_on_an_expired_notification_the_sweep_has_not_reached_starts_a_new_one() {
        let fixture = fixture(ChannelPolicy::default());
        let caller = Caller::member(fixture.alice.clone());
        let external = ExternalId::parse("run-4521").unwrap();
        let upsert = || UpsertRequest {
            channel: channel_id(),
            external_id: external.clone(),
            draft: RevisionDraft {
                ttl: Some(notify_domain::Ttl::parse("1h", "ttl").unwrap()),
                ..RevisionDraft::simple(ContentDraft::titled("nightly backup"))
            },
        };

        let first = fixture.upsert.execute(&caller, upsert()).await.unwrap();
        // Past its retention, and no purge has run.
        fixture.clock.advance(Duration::hours(2));
        let second = fixture.upsert.execute(&caller, upsert()).await.unwrap();

        assert_ne!(
            first.notification.id, second.notification.id,
            "a revision there would land on something no reader can see"
        );
        assert_eq!(second.revision.seq, 1);
    }

    #[tokio::test]
    async fn critical_t16_a_replay_is_answered_even_once_the_quota_is_spent() {
        let fixture = fixture(ChannelPolicy::default());
        let caller = Caller::member(fixture.alice.clone());

        let first = fixture
            .publish
            .execute(&caller, request(Some("deploy-1")))
            .await
            .unwrap();
        fixture.limiter.limit(Quota::PublishPerKey {
            key: fixture.alice.reference(),
        });

        let retried = fixture
            .publish
            .execute(&caller, request(Some("deploy-1")))
            .await
            .unwrap();

        assert!(
            retried.replayed,
            "replaying costs nothing, so it is not a 429"
        );
        assert_eq!(retried.notification.id, first.notification.id);
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
    async fn a_channels_configured_ceiling_reaches_the_limiter() {
        // §8.2 promises "défauts globaux, surchargeables par canal (`limits` dans
        // `[[channels.managed]]`)". The number was parsed by the config and read by
        // nobody; this is the wire that makes it mean something. Asserted at the
        // port rather than at the outcome because the enforcement itself lives in
        // the adapter that owns the counter — what this use case owes is passing
        // the channel's number rather than dropping it.
        let fixture = fixture(ChannelPolicy {
            publish_per_minute: Some(30),
            ..ChannelPolicy::default()
        });
        let caller = Caller::member(fixture.alice.clone());

        fixture
            .publish
            .execute(&caller, request(None))
            .await
            .unwrap();

        let seen = fixture.limiter.seen_with_ceilings();
        assert_eq!(
            seen,
            vec![
                (
                    Quota::PublishPerKey {
                        key: fixture.alice.reference()
                    },
                    None
                ),
                (
                    Quota::PublishPerChannel {
                        channel: channel_id().to_string()
                    },
                    Some(30)
                ),
            ],
            "the channel's own ceiling travels; a caller with no key of its own \
             (this one authenticates through membership) carries no per-key one"
        );
    }

    #[tokio::test]
    async fn a_channel_that_configures_nothing_is_measured_against_the_default() {
        let fixture = fixture(ChannelPolicy::default());
        let caller = Caller::member(fixture.alice.clone());

        fixture
            .publish
            .execute(&caller, request(None))
            .await
            .unwrap();

        assert!(
            fixture
                .limiter
                .seen_with_ceilings()
                .iter()
                .all(|(_, configured)| configured.is_none()),
            "`None` is what lets the adapter apply the number §8.2 publishes"
        );
    }

    #[tokio::test]
    async fn a_keys_own_ceiling_reaches_the_limiter_alongside_the_channels() {
        // D24's half of §8.2: a platform-admin's per-key override travels the same
        // way the channel's own `limits` does (`a_channels_configured_ceiling_reaches_the_limiter`),
        // just carried on the grant instead of the channel.
        let fixture = fixture(ChannelPolicy {
            publish_per_minute: Some(30),
            ..ChannelPolicy::default()
        });
        let key = ApiKeyId::generate();
        let capped = Caller {
            principal: fixture.alice.clone(),
            grant: Grant::ApiKey {
                id: key,
                scopes: vec![Scope::Publish],
                channels: vec![channel_id()],
                publish_per_minute: Some(5),
                owner_groups: Vec::new(),
            },
            groups: Vec::new(),
            platform_admin: false,
            expires_at: None,
        };

        fixture
            .publish
            .execute(&capped, request(None))
            .await
            .unwrap();

        assert_eq!(
            fixture.limiter.seen_with_ceilings(),
            vec![
                (
                    Quota::PublishPerKey {
                        key: format!("apikey:{key}")
                    },
                    Some(5)
                ),
                (
                    Quota::PublishPerChannel {
                        channel: channel_id().to_string()
                    },
                    Some(30)
                ),
            ],
            "both ceilings travel to the limiter, independently"
        );
    }

    #[tokio::test]
    async fn critical_t14_one_limited_publisher_does_not_affect_the_others() {
        let fixture = fixture(ChannelPolicy::default());
        let key = ApiKeyId::generate();
        let noisy = Caller {
            principal: fixture.alice.clone(),
            grant: Grant::ApiKey {
                id: key,
                scopes: vec![Scope::Publish],
                channels: vec![channel_id()],
                publish_per_minute: None,
                owner_groups: Vec::new(),
            },
            groups: Vec::new(),
            platform_admin: false,
            expires_at: None,
        };
        fixture.limiter.limit(Quota::PublishPerKey {
            key: format!("apikey:{key}"),
        });

        assert!(
            fixture
                .publish
                .execute(&noisy, request(None))
                .await
                .is_err()
        );

        // Nor is the same owner's other key: the ceiling is this key's (D24).
        let sibling = Caller {
            grant: Grant::ApiKey {
                id: ApiKeyId::generate(),
                scopes: vec![Scope::Publish],
                channels: vec![channel_id()],
                publish_per_minute: None,
                owner_groups: Vec::new(),
            },
            ..noisy.clone()
        };
        assert!(
            fixture
                .publish
                .execute(&sibling, request(None))
                .await
                .is_ok()
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

    // --- D30: a thumbnail is derived eagerly, and never at the cost of the write ---

    fn request_with_attachment(reference: notify_domain::AttachmentId) -> PublishRequest {
        PublishRequest {
            channel: channel_id(),
            draft: RevisionDraft::simple(ContentDraft {
                image: Some(notify_domain::Image::Attachment { reference }),
                ..ContentDraft::titled("screenshot")
            }),
            idempotency_key: None,
        }
    }

    #[tokio::test]
    async fn publishing_with_an_attachment_image_derives_and_stores_a_thumbnail() {
        let fixture = fixture(ChannelPolicy::default());
        let caller = Caller::member(fixture.alice.clone());
        let reference = notify_domain::AttachmentId::generate();
        notify_ports::AttachmentStore::put(
            &fixture.attachments,
            reference,
            "image/png",
            b"source bytes".to_vec(),
        )
        .await
        .unwrap();

        fixture
            .publish
            .execute(&caller, request_with_attachment(reference))
            .await
            .unwrap();

        let thumbnail =
            notify_ports::AttachmentStore::get_thumbnail(&fixture.attachments, reference)
                .await
                .unwrap();
        assert_eq!(
            thumbnail.map(|object| object.bytes),
            Some(b"source bytes".to_vec())
        );
    }

    #[tokio::test]
    async fn a_reference_whose_upload_never_arrived_derives_nothing_and_still_publishes() {
        let fixture = fixture(ChannelPolicy::default());
        let caller = Caller::member(fixture.alice.clone());
        let never_uploaded = notify_domain::AttachmentId::generate();

        let published = fixture
            .publish
            .execute(&caller, request_with_attachment(never_uploaded))
            .await
            .unwrap();

        assert_eq!(published.revision.seq, 1);
        assert!(
            notify_ports::AttachmentStore::get_thumbnail(&fixture.attachments, never_uploaded)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_rejected_source_leaves_no_thumbnail_and_still_publishes() {
        let fixture = fixture_refusing_thumbnails(ChannelPolicy::default());
        let caller = Caller::member(fixture.alice.clone());
        let reference = notify_domain::AttachmentId::generate();
        notify_ports::AttachmentStore::put(
            &fixture.attachments,
            reference,
            "image/gif",
            b"an animated gif, say".to_vec(),
        )
        .await
        .unwrap();

        let published = fixture
            .publish
            .execute(&caller, request_with_attachment(reference))
            .await
            .unwrap();

        assert_eq!(published.revision.seq, 1);
        assert!(
            notify_ports::AttachmentStore::get_thumbnail(&fixture.attachments, reference)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_revision_with_no_image_derives_nothing() {
        let fixture = fixture(ChannelPolicy::default());
        let caller = Caller::member(fixture.alice.clone());

        // `request(None)` carries no image at all — the common case, and the one
        // that must not pay a bucket round trip it has nothing to make.
        fixture
            .publish
            .execute(&caller, request(None))
            .await
            .unwrap();

        // Nothing to assert against a store keyed by an id that was never
        // minted; the property under test is that the call above did not panic
        // or error reaching for `revision.content.image` on `None`.
    }
}
