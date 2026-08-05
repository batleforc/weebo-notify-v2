//! What happens after a write lands.
//!
//! Two things, in this order and for different reasons:
//!
//! 1. **Poke the bus.** Every replica holds its own stream connections, so every
//!    replica has to learn about the write. The poke carries an id and a cursor,
//!    never a payload (ADR-0008).
//! 2. **Push to devices that are not listening.** Filtered by the recipient's own
//!    silence *and* do-not-disturb — the server is where "stop notifying me" is
//!    enforced, so that it holds across every device the person owns (design §2.3,
//!    §7.4). Only the push is suppressed: the bus poke above still goes out, so a
//!    client with the app open keeps receiving on its stream and the inbox fills
//!    normally. An app somebody chose to leave open is not an interruption.
//!
//! Both are best-effort by design. A failed poke costs latency and the cursor
//! catches up; a failed push is recorded and retried by the transport. Neither may
//! fail the publish that caused it: a CI job must not see a 500 because somebody's
//! phone is unreachable.

use std::sync::Arc;

use notify_domain::{Cursor, Device, Notification, Principal, PushTransport};
use notify_ports::{
    Clock, DeviceRepository, EventBus, InteractionRepository, Poke, PreferenceRepository, PushBody,
    PushDispatcher, PushMessage, PushOutcome,
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
    ) -> Self {
        Self {
            bus,
            devices,
            push,
            interactions,
            preferences,
            clock,
        }
    }

    /// Tell the instance a channel event landed. Never fails the caller.
    pub async fn poke_channel(&self, notification: &Notification, cursor: Cursor) {
        let poke = Poke::Channel {
            id: notification.channel.clone(),
            cursor,
        };
        if let Err(error) = self.bus.publish(&poke).await {
            // A dropped poke costs latency, never correctness: the client's cursor
            // recovers it on the next reconnect (ADR-0008).
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

    /// Push a notification to the devices that should hear about it.
    ///
    /// Errors are logged, never propagated. Reporting a publish as failed because a
    /// push relay was unhappy would make a CI pipeline red for somebody else's
    /// phone.
    pub async fn push(&self, notification: &Notification, max_payload_hint: usize) {
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
            // A device with no push transport is reachable only while its client
            // holds a stream, which the bus poke already covered.
            if matches!(device.transport, PushTransport::Stream) {
                continue;
            }

            let message = Self::message_for(notification, max_payload_hint);
            match self.push.dispatch(&device, &message).await {
                Ok(PushOutcome::Delivered) => {}
                Ok(PushOutcome::Expired) => {
                    // The relay says this subscription is gone. Retrying it until
                    // the notification expires would help nobody.
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

    /// Whether this device's owner has asked not to be bothered by this — by a
    /// silence (design §2.3) or by a do-not-disturb (§7.4).
    ///
    /// Both are read here rather than in one place because they answer different
    /// questions about the same moment: a silence is *this notification or this
    /// group*, a do-not-disturb is *this channel, at this priority, right now*.
    /// Neither reaches the stream: only the push is suppressed (§7.4).
    async fn stays_quiet(&self, device: &Device, notification: &Notification) -> bool {
        // Both are per user; a service account has no attention to protect
        // (design §5.2bis).
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
                // Failing open would buzz a phone somebody asked to stay quiet.
                // Failing closed loses one push, which the inbox still holds.
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
            // Counted here and nowhere else: the digest that ends the quiet period
            // says how many were held back (§7.4), and this is the only moment that
            // knows one was. Failing to count costs a number in a message; failing
            // the publish over it would be absurd.
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
                // Sealing to the device key lands with M6 (design §6.2); until
                // then no transport that needs it is wired.
                sealed: false,
            });

        PushMessage::Notification {
            channel: notification.channel.clone(),
            notification_id: notification.id,
            priority: notification.priority(),
            body,
        }
    }

    /// Send one device the digest that ends a quiet period (design §7.4).
    ///
    /// Here rather than in the job because this is where "talking to a device" lives
    /// — including the part the job would otherwise duplicate: a relay reporting a
    /// dead subscription revokes it, whatever the message was.
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
        Fanout::new(
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(clock.clone()),
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
        fanout.push(&notification, 4_096).await;

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
            .push(&notification(&alice, None, clock.now()), 4_096)
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
            .push(
                &notification(&bob, Some("incident-4521"), clock.now()),
                4_096,
            )
            .await;

        assert_eq!(
            store.pushes().len(),
            1,
            "Bob still hears about it; Alice asked not to"
        );
    }

    // --- T19 · Do not disturb (design §3.4, §7.4) --------------------------

    /// The clause that matters most, and the one an implementation gets wrong by
    /// suppressing too much: the push stops, the stream does not.
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
        fanout.push(&notification, 4_096).await;

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
            .push(&notification(&alice, None, clock.now()), 4_096)
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

        fanout(&store, &clock).push(&urgent, 4_096).await;

        assert!(
            store.pushes().is_empty(),
            "the recipient decides what wakes them, not the publisher (§2.3, §7.4)"
        );
    }

    #[tokio::test]
    async fn a_service_account_device_is_never_quieted_by_a_preference() {
        // A bot has no attention to protect (§5.2bis), so there is nothing to read
        // and nothing that could suppress its delivery.
        let store = InMemory::new();
        let clock = TestClock::new();
        let bot = Principal::ServiceAccount {
            id: notify_domain::ServiceAccountId::parse("bot-argocd").unwrap(),
        };
        seed(&store, &bot, webpush(), clock.now());

        fanout(&store, &clock)
            .push(&notification(&bot, None, clock.now()), 4_096)
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
            .push(&notification(&alice, None, clock.now()), 4_096)
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
        fanout(&store, &clock)
            .push(&notification(&alice, None, clock.now()), 4)
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
            .push(&notification(&alice, None, clock.now()), 4_096)
            .await;
        assert_eq!(store.pushes().len(), 1);
    }
}
