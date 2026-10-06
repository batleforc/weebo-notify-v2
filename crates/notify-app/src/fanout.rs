//! What happens after a write lands: poke the bus (id + cursor, ADR-0008), push to
//! devices not silenced or in DND (§2.3, §7.4; only the push is suppressed, never the
//! stream), queue the channel's sinks (§7.1bis, no per-user filtering), then derive a
//! thumbnail. All best-effort: none may fail the publish that caused it.

use std::sync::Arc;

use notify_domain::{
    Cursor, DeliveryId, Device, Image, Notification, Principal, PushTransport, Revision,
};
use notify_ports::{
    AttachmentStore, AttachmentThumbnailer, Audience, Clock, DeviceRepository, Event, EventBus,
    EventPayload, InteractionRepository, Poke, PreferenceRepository, PushBody, PushDispatcher,
    PushMessage, PushOutcome, RevisionEvent, SinkDelivery, SinkQueue, SinkRegistry,
};

/// Broadcasts a write to the rest of the instance and to devices.
#[derive(Clone)]
pub struct Fanout {
    bus: Arc<dyn EventBus>,
    devices: Arc<dyn DeviceRepository>,
    push: Arc<dyn PushDispatcher>,
    interactions: Arc<dyn InteractionRepository>,
    preferences: Arc<dyn PreferenceRepository>,
    clock: Arc<dyn Clock>,
    /// The largest payload the configured transports will carry; past it a push degrades
    /// to a poke (§6.3). Zero with no push transport: every message is a poke.
    payload_budget: usize,
    /// Where a channel's events go besides devices, and their queue (§7.1bis); `None`
    /// when no sink is declared.
    sinks: Option<Sinks>,
    /// Attachment store and thumbnailer; `None` with no object store.
    thumbnails: Option<Thumbnails>,
}

/// The two halves of the sink route, wired or not as one.
#[derive(Clone)]
struct Sinks {
    registry: Arc<dyn SinkRegistry>,
    queue: Arc<dyn SinkQueue>,
}

/// What deriving a thumbnail needs, wired or not as one.
#[derive(Clone)]
struct Thumbnails {
    store: Arc<dyn AttachmentStore>,
    thumbnailer: Arc<dyn AttachmentThumbnailer>,
}

impl core::fmt::Debug for Fanout {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Fanout").finish_non_exhaustive()
    }
}

impl Fanout {
    /// Build one.
    #[must_use]
    pub fn new(
        bus: Arc<dyn EventBus>,
        devices: Arc<dyn DeviceRepository>,
        push: Arc<dyn PushDispatcher>,
        interactions: Arc<dyn InteractionRepository>,
        preferences: Arc<dyn PreferenceRepository>,
        clock: Arc<dyn Clock>,
        payload_budget: usize,
    ) -> Self {
        Self {
            bus,
            devices,
            push,
            interactions,
            preferences,
            clock,
            payload_budget,
            sinks: None,
            thumbnails: None,
        }
    }

    /// Route this channel's events to the configured sinks as well (§7.1bis).
    #[must_use]
    pub fn with_sinks(
        mut self,
        registry: Arc<dyn SinkRegistry>,
        queue: Arc<dyn SinkQueue>,
    ) -> Self {
        self.sinks = Some(Sinks { registry, queue });
        self
    }

    /// Derive an attachment's thumbnail after a write as well.
    #[must_use]
    pub fn with_thumbnails(
        mut self,
        store: Arc<dyn AttachmentStore>,
        thumbnailer: Arc<dyn AttachmentThumbnailer>,
    ) -> Self {
        self.thumbnails = Some(Thumbnails { store, thumbnailer });
        self
    }

    /// Tell the instance a channel event landed. Never fails the caller.
    pub async fn poke_channel(&self, notification: &Notification, cursor: Cursor) {
        let poke = Poke::Channel {
            id: notification.channel.clone(),
            cursor,
        };
        if let Err(error) = self.bus.publish(&poke).await {
            // A dropped poke costs latency, never correctness: the cursor recovers it (ADR-0008).
            tracing::warn!(
                ?error,
                "event bus poke failed; clients will catch up by cursor"
            );
        }
    }

    /// Tell one user their own preference state changed (design §8.1).
    pub async fn poke_user(&self, user: notify_domain::UserId, cursor: Cursor) {
        if let Err(error) = self.bus.publish(&Poke::User { id: user, cursor }).await {
            tracing::warn!(?error, "event bus poke failed");
        }
    }

    /// Queue this revision for the channel's outbound sinks (§7.1bis). The event is
    /// rebuilt from what the store appended it from, so consumers see the stream's bytes.
    pub async fn queue_sinks(
        &self,
        notification: &Notification,
        revision: &Revision,
        cursor: Cursor,
    ) {
        let Some(sinks) = &self.sinks else {
            return;
        };

        let targets = sinks.registry.sinks_for(&notification.channel);
        if targets.is_empty() {
            return;
        }

        let event = Event {
            cursor,
            audience: Audience::Channel {
                id: notification.channel.clone(),
            },
            recorded_at: revision.received_at,
            // Per-delivery fact; the log is read by everybody (§7.4).
            muted_by_dnd: false,
            payload: EventPayload::Revision(Box::new(RevisionEvent::new(notification, revision))),
        };

        for sink in targets {
            let delivery = SinkDelivery {
                // One id per sink: `X-Weebo-Delivery` is each consumer's idempotency key.
                delivery_id: DeliveryId::generate(),
                channel: notification.channel.clone(),
                sink,
                event: event.clone(),
                attempts: 0,
                first_failed_at: None,
            };

            // Due immediately: the wait is the drain interval, not a backoff.
            if let Err(error) = sinks.queue.enqueue(&delivery, self.clock.now()).await {
                // The publish is already durable; the delivery is lost, loudly.
                tracing::error!(
                    ?error,
                    channel = %notification.channel,
                    sink = %delivery.sink,
                    "could not queue a sink delivery; it is lost"
                );
            }
        }
    }

    /// Derive this revision's attachment thumbnail, if it shows one. Best-effort: no
    /// store, a missing upload or an undecodable image just means no thumbnail.
    pub async fn derive_thumbnail(&self, revision: &Revision) {
        let Some(thumbnails) = &self.thumbnails else {
            return;
        };
        let Some(Image::Attachment { reference }) = &revision.content.image else {
            return;
        };
        let reference = *reference;

        let object = match thumbnails.store.get(reference).await {
            Ok(Some(object)) => object,
            Ok(None) => return,
            Err(error) => {
                tracing::debug!(%error, attachment = %reference, "thumbnail source unavailable");
                return;
            }
        };

        let Ok(Ok(processed)) = thumbnails.thumbnailer.thumbnail(&object.bytes).await else {
            return;
        };

        if let Err(error) = thumbnails
            .store
            .put_thumbnail(reference, &processed.content_type, processed.bytes)
            .await
        {
            tracing::warn!(%error, attachment = %reference, "thumbnail derived but not stored");
        }
    }

    /// Push a notification to the devices that should hear about it. Errors are logged,
    /// never propagated; the span is what explains "published but nothing arrived" (§13).
    #[tracing::instrument(
        name = "fanout",
        skip_all,
        fields(channel = %notification.channel, notification = %notification.id)
    )]
    pub async fn push(&self, notification: &Notification) {
        let targets = match self.devices.delivery_targets(&notification.channel).await {
            Ok(targets) => targets,
            Err(error) => {
                tracing::warn!(?error, "could not resolve delivery targets");
                return;
            }
        };

        for device in targets {
            if self.stays_quiet(&device, notification).await {
                continue;
            }
            // A stream-only device was already covered by the bus poke.
            if matches!(device.transport, PushTransport::Stream) {
                continue;
            }

            let message = Self::message_for(notification, self.payload_budget);
            match self.push.dispatch(&device, &message).await {
                Ok(PushOutcome::Delivered) => {}
                Ok(PushOutcome::Expired) => {
                    // The relay says this subscription is gone.
                    let now = self.clock.now();
                    if let Err(error) = self.devices.revoke(device.id, now).await {
                        tracing::warn!(?error, device = %device.id, "could not revoke a dead device");
                    }
                }
                Ok(PushOutcome::Failed { reason }) => {
                    tracing::warn!(device = %device.id, reason, "push failed");
                }
                Err(error) => {
                    tracing::warn!(?error, device = %device.id, "push transport unusable");
                }
            }
        }
    }

    /// Whether the device's owner asked not to be bothered: a silence (§2.3) or a
    /// do-not-disturb (§7.4). Only the push is suppressed.
    async fn stays_quiet(&self, device: &Device, notification: &Notification) -> bool {
        // Both are per user; a service account has no attention to protect (§5.2bis).
        let Principal::User { id } = device.owner else {
            return false;
        };
        let now = self.clock.now();

        match self.interactions.active_silences(id, now).await {
            Ok(active) => {
                let covered = active.iter().any(|silence| {
                    silence.covers(
                        notification.id,
                        notification.head.content.group_key.as_deref(),
                    )
                });
                if covered {
                    return true;
                }
            }
            Err(error) => {
                // Fail closed: one lost push (the inbox still holds it) beats buzzing a quiet phone.
                tracing::warn!(?error, "could not read silences; suppressing the push");
                return true;
            }
        }

        let muted = match self.preferences.dnd(id).await {
            Ok(dnd) => dnd.is_muted(&notification.channel, notification.priority(), now),
            Err(error) => {
                tracing::warn!(
                    ?error,
                    "could not read do-not-disturb; suppressing the push"
                );
                return true;
            }
        };

        if muted {
            // The only moment that knows a push was held back; the closing digest counts them (§7.4).
            if let Err(error) = self.preferences.defer(id, notification.head.status).await {
                tracing::warn!(?error, "could not record a deferred push for the digest");
            }
        }

        muted
    }

    /// Build the push body, degrading to a poke when the payload will not fit.
    fn message_for(notification: &Notification, max_payload: usize) -> PushMessage {
        let body = serde_json::to_vec(&notification.head.content)
            .ok()
            .filter(|bytes| bytes.len() <= max_payload)
            .map_or(PushBody::Poke, |bytes| PushBody::Payload {
                bytes,
                // Sealing is the relayed transport's job, against `device.public_key` (§6.2).
                sealed: false,
            });

        PushMessage::Notification {
            channel: notification.channel.clone(),
            notification_id: notification.id,
            priority: notification.priority(),
            body,
        }
    }

    /// Send one device the digest that ends a quiet period (§7.4); a dead subscription
    /// is revoked here, as for any push.
    pub async fn push_digest(&self, device: &Device, message: &PushMessage) {
        if matches!(device.transport, PushTransport::Stream) {
            return;
        }

        match self.push.dispatch(device, message).await {
            Ok(PushOutcome::Delivered) => {}
            Ok(PushOutcome::Expired) => {
                let now = self.clock.now();
                if let Err(error) = self.devices.revoke(device.id, now).await {
                    tracing::warn!(?error, device = %device.id, "could not revoke a dead device");
                }
            }
            Ok(PushOutcome::Failed { reason }) => {
                tracing::warn!(device = %device.id, reason, "digest push failed");
            }
            Err(error) => {
                tracing::warn!(?error, device = %device.id, "push transport unusable");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        reason = "a failing assertion is a failing test"
    )]

    use notify_domain::{
        Channel, ChannelId, ChannelPolicy, ContentDraft, DeviceId, Dnd, HumanDuration, Membership,
        NotificationId, RevisionDraft, Scope, Silence, SilenceTarget, State, UserId,
    };
    use notify_ports::test_utils::{InMemory, TestClock};
    use url::Url;

    use super::*;

    fn fanout(store: &InMemory, clock: &TestClock) -> Fanout {
        fanout_carrying(store, clock, 4_096)
    }

    /// One whose transports carry `budget` bytes, for the tests about the ceiling.
    fn fanout_carrying(store: &InMemory, clock: &TestClock, budget: usize) -> Fanout {
        Fanout::new(
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(clock.clone()),
            budget,
        )
    }

    fn channel_id() -> ChannelId {
        ChannelId::parse("ci-infra").unwrap()
    }

    fn notification(
        user: &Principal,
        group_key: Option<&str>,
        at: time::OffsetDateTime,
    ) -> Notification {
        let content = ContentDraft {
            group_key: group_key.map(str::to_owned),
            ..ContentDraft::titled("etcd degraded")
        };
        Notification::create(
            NotificationId::generate(),
            channel_id(),
            None,
            RevisionDraft::lifecycle(State::Open, content),
            notify_domain::Author::anonymous(user.clone()),
            at,
            &ChannelPolicy::default(),
        )
        .unwrap()
        .0
    }

    fn seed(
        store: &InMemory,
        user: &Principal,
        transport: PushTransport,
        at: time::OffsetDateTime,
    ) -> DeviceId {
        let mut channel = Channel::dynamic(channel_id(), None).unwrap();
        channel.upsert_member(Membership::new(user.clone(), vec![Scope::Subscribe]).unwrap());
        store.seed_channel(channel);

        let device = Device::enrol(
            DeviceId::generate(),
            user.clone(),
            "phone",
            transport,
            None,
            at,
        )
        .unwrap();
        let id = device.id;
        store.seed_device(device);
        id
    }

    fn webpush() -> PushTransport {
        PushTransport::WebPush {
            endpoint: Url::parse("https://push.example/abc").unwrap(),
            p256dh: "BPk".to_owned(),
            auth: "k9".to_owned(),
        }
    }

    #[tokio::test]
    async fn a_publish_pokes_the_bus_and_pushes_to_a_subscribed_device() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let alice = Principal::User {
            id: UserId::generate(),
        };
        seed(&store, &alice, webpush(), clock.now());

        let notification = notification(&alice, None, clock.now());
        let fanout = fanout(&store, &clock);

        fanout
            .poke_channel(&notification, Cursor::from_sequence(1))
            .await;
        fanout.push(&notification).await;

        assert_eq!(store.pokes().len(), 1);
        assert_eq!(store.pushes().len(), 1);
    }

    #[tokio::test]
    async fn a_stream_only_device_is_never_pushed_to() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let alice = Principal::User {
            id: UserId::generate(),
        };
        seed(&store, &alice, PushTransport::Stream, clock.now());

        fanout(&store, &clock)
            .push(&notification(&alice, None, clock.now()))
            .await;

        assert!(
            store.pushes().is_empty(),
            "the bus poke already reached everything holding a stream"
        );
    }

    // --- T20 · Silence (design §3.4) --------------------------------------

    #[tokio::test]
    async fn critical_t20_a_group_silence_suppresses_the_push_for_that_user_only() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let (alice, bob) = (
            Principal::User {
                id: UserId::generate(),
            },
            Principal::User {
                id: UserId::generate(),
            },
        );

        // Both are members with a push-capable device.
        let mut channel = Channel::dynamic(channel_id(), None).unwrap();
        for member in [&alice, &bob] {
            channel.upsert_member(Membership::new(member.clone(), vec![Scope::Subscribe]).unwrap());
            store.seed_device(
                Device::enrol(
                    DeviceId::generate(),
                    member.clone(),
                    "phone",
                    webpush(),
                    None,
                    clock.now(),
                )
                .unwrap(),
            );
        }
        store.seed_channel(channel);

        // Alice silences the group.
        let Principal::User { id: alice_id } = alice else {
            unreachable!()
        };
        let silence = Silence::start(
            alice_id,
            SilenceTarget::Group {
                key: "incident-4521".to_owned(),
            },
            Some(HumanDuration::from_secs_const(4 * 3_600)),
            clock.now(),
        )
        .unwrap();
        InteractionRepository::silence(&store, &silence)
            .await
            .unwrap();

        fanout(&store, &clock)
            .push(&notification(&bob, Some("incident-4521"), clock.now()))
            .await;

        assert_eq!(
            store.pushes().len(),
            1,
            "Bob still hears about it; Alice asked not to"
        );
    }

    // --- T19 · Do not disturb (design §3.4, §7.4) --------------------------

    /// The push stops, the stream does not.
    #[tokio::test]
    async fn critical_t19_a_quiet_period_stops_the_push_and_leaves_the_stream_alone() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let alice = Principal::User {
            id: UserId::generate(),
        };
        let Principal::User { id: alice_id } = alice else {
            unreachable!()
        };
        seed(&store, &alice, webpush(), clock.now());

        // Quiet from now until well past the publish.
        PreferenceRepository::set_dnd(
            &store,
            &Dnd {
                until: Some(clock.now() + time::Duration::hours(8)),
                ..Dnd::off(alice_id)
            },
            clock.now(),
        )
        .await
        .unwrap();

        let notification = notification(&alice, None, clock.now());
        let fanout = fanout(&store, &clock);
        fanout
            .poke_channel(&notification, Cursor::from_sequence(1))
            .await;
        fanout.push(&notification).await;

        assert!(
            store.pushes().is_empty(),
            "a quiet period means no OS push (§7.4)"
        );
        assert!(
            store
                .pokes()
                .iter()
                .any(|poke| matches!(poke, Poke::Channel { .. })),
            "the stream keeps flowing: an app somebody left open is not an interruption"
        );
    }

    #[tokio::test]
    async fn critical_t19_an_excepted_channel_rings_through_a_quiet_period() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let alice = Principal::User {
            id: UserId::generate(),
        };
        let Principal::User { id: alice_id } = alice else {
            unreachable!()
        };
        seed(&store, &alice, webpush(), clock.now());

        PreferenceRepository::set_dnd(
            &store,
            &Dnd {
                until: Some(clock.now() + time::Duration::hours(8)),
                // The on-call case: mute everything *except* this one.
                exceptions: vec![channel_id()],
                ..Dnd::off(alice_id)
            },
            clock.now(),
        )
        .await
        .unwrap();

        fanout(&store, &clock)
            .push(&notification(&alice, None, clock.now()))
            .await;

        assert_eq!(store.pushes().len(), 1);
    }

    #[tokio::test]
    async fn critical_t19_urgent_does_not_get_through_on_its_own_authority() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let alice = Principal::User {
            id: UserId::generate(),
        };
        let Principal::User { id: alice_id } = alice else {
            unreachable!()
        };
        seed(&store, &alice, webpush(), clock.now());

        PreferenceRepository::set_dnd(
            &store,
            &Dnd {
                until: Some(clock.now() + time::Duration::hours(8)),
                ..Dnd::off(alice_id)
            },
            clock.now(),
        )
        .await
        .unwrap();

        // The publisher asks as loudly as the model allows.
        let urgent = Notification::create(
            NotificationId::generate(),
            channel_id(),
            None,
            RevisionDraft {
                priority: Some(notify_domain::Priority::Urgent),
                ..RevisionDraft::lifecycle(State::Open, ContentDraft::titled("etcd is gone"))
            },
            notify_domain::Author::anonymous(alice.clone()),
            clock.now(),
            &ChannelPolicy::default(),
        )
        .unwrap()
        .0;

        fanout(&store, &clock).push(&urgent).await;

        assert!(
            store.pushes().is_empty(),
            "the recipient decides what wakes them, not the publisher (§2.3, §7.4)"
        );
    }

    #[tokio::test]
    async fn a_service_account_device_is_never_quieted_by_a_preference() {
        // A bot has no attention to protect (§5.2bis).
        let store = InMemory::new();
        let clock = TestClock::new();
        let bot = Principal::ServiceAccount {
            id: notify_domain::ServiceAccountId::parse("bot-argocd").unwrap(),
        };
        seed(&store, &bot, webpush(), clock.now());

        fanout(&store, &clock)
            .push(&notification(&bot, None, clock.now()))
            .await;

        assert_eq!(store.pushes().len(), 1);
    }

    #[tokio::test]
    async fn a_relay_reporting_a_dead_subscription_revokes_the_device() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let alice = Principal::User {
            id: UserId::generate(),
        };
        let device = seed(&store, &alice, webpush(), clock.now());
        store.set_push_outcome(PushOutcome::Expired);

        fanout(&store, &clock)
            .push(&notification(&alice, None, clock.now()))
            .await;

        let stored = DeviceRepository::get(&store, device)
            .await
            .unwrap()
            .unwrap();
        assert!(
            !stored.is_active(),
            "retrying an uninstalled app until the notification expires helps nobody"
        );
    }

    #[tokio::test]
    async fn a_payload_that_will_not_fit_degrades_to_a_poke() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let alice = Principal::User {
            id: UserId::generate(),
        };
        seed(&store, &alice, webpush(), clock.now());

        // A ceiling no real payload fits under.
        fanout_carrying(&store, &clock, 4)
            .push(&notification(&alice, None, clock.now()))
            .await;

        let (_, message) = store.pushes().into_iter().next().unwrap();
        let PushMessage::Notification { body, .. } = message else {
            panic!("a publish pushes a notification, not a digest");
        };
        assert_eq!(
            body,
            PushBody::Poke,
            "the client wakes and fetches over TLS instead"
        );
    }

    #[tokio::test]
    async fn a_deployment_with_no_transport_configured_sends_pokes_and_not_payloads() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let alice = Principal::User {
            id: UserId::generate(),
        };
        seed(&store, &alice, webpush(), clock.now());

        // What `TransportRegistry::max_payload_bytes` answers when nothing is configured.
        fanout_carrying(&store, &clock, 0)
            .push(&notification(&alice, None, clock.now()))
            .await;

        let (_, message) = store.pushes().into_iter().next().unwrap();
        let PushMessage::Notification { body, .. } = message else {
            panic!("a publish pushes a notification, not a digest");
        };
        assert_eq!(body, PushBody::Poke);
    }

    // --- Outbound sinks (design §7.1bis) ----------------------------------

    #[tokio::test]
    async fn a_publish_queues_one_delivery_per_configured_sink() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let alice = Principal::User {
            id: UserId::generate(),
        };
        seed(&store, &alice, PushTransport::Stream, clock.now());

        let registry = notify_ports::test_utils::StaticSinkRegistry::empty()
            .route(
                channel_id(),
                notify_ports::SinkTarget::Webhook {
                    url: Url::parse("https://hooks.weebo.poc/incidents").unwrap(),
                    secret: "shh".to_owned(),
                },
            )
            .route(
                channel_id(),
                notify_ports::SinkTarget::Email {
                    address: "oncall@weebo.poc".to_owned(),
                },
            );

        let notification = notification(&alice, None, clock.now());
        let revision = notification.head.clone();
        fanout(&store, &clock)
            .with_sinks(Arc::new(registry), Arc::new(store.clone()))
            .queue_sinks(&notification, &revision, Cursor::from_sequence(1))
            .await;

        let queued = notify_ports::SinkQueue::claim_due(
            &store,
            clock.now(),
            10,
            clock.now() + time::Duration::minutes(1),
        )
        .await
        .unwrap();

        assert_eq!(queued.len(), 2, "one per sink, not one per publish");
        assert_ne!(
            queued[0].delivery_id, queued[1].delivery_id,
            "two consumers sharing an idempotency key would make each other's \
             deduplication wrong"
        );
        assert_eq!(
            queued[0].event.cursor,
            Cursor::from_sequence(1),
            "a consumer sees what a client sees on the stream (§8.1)"
        );
    }

    #[tokio::test]
    async fn a_channel_with_no_sink_queues_nothing() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let alice = Principal::User {
            id: UserId::generate(),
        };
        seed(&store, &alice, PushTransport::Stream, clock.now());

        let notification = notification(&alice, None, clock.now());
        let revision = notification.head.clone();
        fanout(&store, &clock)
            .with_sinks(
                Arc::new(notify_ports::test_utils::StaticSinkRegistry::empty()),
                Arc::new(store.clone()),
            )
            .queue_sinks(&notification, &revision, Cursor::from_sequence(1))
            .await;

        assert!(
            notify_ports::SinkQueue::claim_due(&store, clock.now(), 10, clock.now())
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_push_failure_never_reaches_the_publisher() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let alice = Principal::User {
            id: UserId::generate(),
        };
        seed(&store, &alice, webpush(), clock.now());
        store.set_push_outcome(PushOutcome::Failed { reason: "timeout" });

        // The point of the test is that this returns at all.
        fanout(&store, &clock)
            .push(&notification(&alice, None, clock.now()))
            .await;
        assert_eq!(store.pushes().len(), 1);
    }
}
