//! What happens after a write lands. The write commits what it owes in its own
//! transaction (ADR-0025): sink deliveries (§7.1bis, no per-user filtering), the
//! idempotent replay, and one outbox row for the device push and thumbnail. The request
//! then pokes the bus (id + cursor, ADR-0008) and wakes [`DrainFanout`], which pushes to
//! devices not silenced or in DND (§2.3, §7.4; only the push is suppressed, never the
//! stream) and derives the thumbnail. At least once; none may fail the publish.

use std::sync::Arc;

use futures::StreamExt as _;
use notify_domain::{
    AttachmentId, Cursor, Device, DeviceId, Image, Notification, Principal, PushTransport, Revision,
};
use notify_ports::{
    AfterWrite, AttachmentStore, AttachmentThumbnailer, Clock, DeviceRepository, EventBus,
    FanoutOutbox, InteractionRepository, NotificationRepository, PendingFanout, Poke, PortResult,
    PreferenceRepository, PushBody, PushDispatcher, PushMessage, PushOutcome, SinkRegistry,
};

/// How many devices one notification is pushed to at once.
///
/// Serially, a channel with fifty phones behind slow relays took fifty timeouts.
/// Bounded, so one big channel cannot open a connection per device in one burst.
const PUSH_CONCURRENCY: usize = 16;

/// How many outbox rows the worker claims per round, and works on at once.
const OUTBOX_BATCH: u32 = 8;

/// How long a claimed outbox row is the claimant's, from its claim or its last
/// heartbeat.
///
/// Short enough that a dead worker's rows come back within minutes; the
/// [`LEASE_HEARTBEAT`] keeps a live one's for as long as it is still pushing, so a
/// big channel is never raced by a second worker for the same row.
const OUTBOX_LEASE: time::Duration = time::Duration::minutes(5);

/// How often a worker still pushing a row extends its lease back to
/// [`OUTBOX_LEASE`]. Well inside it, so one missed beat costs nothing.
const LEASE_HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(60);

/// The longest one notification's push and thumbnail may take before the row is
/// given back to be retried — a safety net, not a schedule.
///
/// Each delivery is already capped by the transports (`notify_push_http::retry::MAX_TOTAL`) and
/// the lease is extended while the worker lives, so this only bites on a worker
/// that is wedged rather than slow; the retry then skips the devices already
/// pushed.
const ROW_CEILING: std::time::Duration = std::time::Duration::from_mins(15);

/// Claims a row gets before it is retired as dead rather than tried again.
const MAX_ATTEMPTS: u32 = 10;

/// How long a row given back unfinished waits, per attempt so far.
const RETRY_STEP: time::Duration = time::Duration::seconds(30);

/// Past this, a push is noise rather than news: the row is retired as dead instead
/// of buzzing a phone about something an hour old (the inbox already holds it).
const OUTBOX_MAX_AGE: time::Duration = time::Duration::hours(1);

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
    /// Where a channel's events go besides devices (§7.1bis); the write queues them in
    /// its own transaction. `None` when no sink is declared.
    sinks: Option<Arc<dyn SinkRegistry>>,
    /// Attachment store and thumbnailer; `None` with no object store.
    thumbnails: Option<Thumbnails>,
    /// Rung by a publish once its outbox row is committed, so the worker on this
    /// replica pushes now rather than at its next tick. A permit is kept when
    /// nobody waits, so a wake during a round is not lost.
    wake: Arc<tokio::sync::Notify>,
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
            wake: Arc::default(),
        }
    }

    /// Route this channel's events to the configured sinks as well (§7.1bis).
    #[must_use]
    pub fn with_sinks(mut self, registry: Arc<dyn SinkRegistry>) -> Self {
        self.sinks = Some(registry);
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

    /// What a write of `revision` owes once it commits (ADR-0025): device push,
    /// thumbnail and one delivery per sink. The caller adds the idempotent reply.
    #[must_use]
    pub fn after_write(&self, notification: &Notification, revision: &Revision) -> AfterWrite {
        let thumbnail = match (&self.thumbnails, &revision.content.image) {
            (Some(_), Some(Image::Attachment { reference })) => Some(*reference),
            _ => None,
        };
        AfterWrite {
            fanout: true,
            thumbnail,
            sinks: self
                .sinks
                .as_ref()
                .map(|registry| registry.sinks_for(&notification.channel))
                .unwrap_or_default(),
            reply: None,
        }
    }

    /// Tell this replica's [`DrainFanout`] a row is waiting.
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    /// Derive an attachment's thumbnail. Best-effort: a missing upload or an undecodable
    /// image just means no thumbnail. Idempotent, so a row processed twice is harmless.
    pub async fn derive_thumbnail(&self, reference: AttachmentId) {
        let Some(thumbnails) = &self.thumbnails else {
            return;
        };

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
    pub async fn push(&self, notification: &Notification) {
        self.push_remaining(notification, &[], None).await;
    }

    /// [`Fanout::push`], to every target but `already`, recording each delivery in
    /// `progress` so a retried outbox row resumes rather than starts over (ADR-0025).
    #[tracing::instrument(
        name = "fanout",
        skip_all,
        fields(channel = %notification.channel, notification = %notification.id)
    )]
    async fn push_remaining(
        &self,
        notification: &Notification,
        already: &[DeviceId],
        progress: Option<&dyn FanoutOutbox>,
    ) {
        let mut targets = match self.devices.delivery_targets(&notification.channel).await {
            Ok(targets) => targets,
            Err(error) => {
                tracing::warn!(?error, "could not resolve delivery targets");
                return;
            }
        };
        targets.retain(|device| !already.contains(&device.id));

        // One message for all; each transport degrades to a poke on its own (§6.3).
        let message = Self::message_for(notification, self.payload_budget);
        futures::stream::iter(targets)
            .for_each_concurrent(PUSH_CONCURRENCY, |device| {
                let message = &message;
                async move {
                    let id = device.id;
                    if !self.push_one(device, notification, message).await {
                        return;
                    }
                    // Recorded per send, so a row that dies half way keeps what it did.
                    let Some(outbox) = progress else { return };
                    if let Err(error) = outbox
                        .record_delivery(notification.id, notification.head.seq, id)
                        .await
                    {
                        tracing::warn!(?error, device = %id, "push not recorded; a retry repeats it");
                    }
                }
            })
            .await;
    }

    /// Push to one device, unless its owner asked for quiet, and say whether the
    /// relay took it.
    async fn push_one(
        &self,
        device: Device,
        notification: &Notification,
        message: &PushMessage,
    ) -> bool {
        if self.stays_quiet(&device, notification).await {
            return false;
        }
        // A device with no push transport is reachable only while its client holds
        // a stream, which the bus poke already covered.
        if matches!(device.transport, PushTransport::Stream) {
            return false;
        }

        match self.push.dispatch(&device, message).await {
            Ok(PushOutcome::Delivered) => return true,
            Ok(PushOutcome::Expired) => {
                // The relay says this subscription is gone. Retrying it until the
                // notification expires would help nobody.
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
        false
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
            // Keyed by revision, so a retried row or a second device counts once.
            if let Err(error) = self
                .preferences
                .defer(
                    id,
                    notification.id,
                    notification.head.seq,
                    notification.head.status,
                )
                .await
            {
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

/// The fan-out outbox worker (ADR-0025): pushes and thumbnails for committed
/// writes, on whichever replica claims them first.
///
/// Not leader-only: rows are leased with `SKIP LOCKED`, so every replica can drain
/// its own publishes the moment it is woken, and any replica picks up the rows of
/// one that died.
///
/// A row says *which* notification is owed a push, not what to push: the worker
/// re-reads it, so a delete or an expiry since the write pushes nothing, and the
/// rows of one notification claimed together push its current head once.
#[derive(Clone)]
pub struct DrainFanout {
    outbox: Arc<dyn FanoutOutbox>,
    notifications: Arc<dyn NotificationRepository>,
    fanout: Fanout,
}

impl core::fmt::Debug for DrainFanout {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DrainFanout").finish_non_exhaustive()
    }
}

impl DrainFanout {
    /// Wire it — with the same [`Fanout`] the publish use cases hold, so their
    /// wake reaches this worker.
    #[must_use]
    pub fn new(
        outbox: Arc<dyn FanoutOutbox>,
        notifications: Arc<dyn NotificationRepository>,
        fanout: Fanout,
    ) -> Self {
        Self {
            outbox,
            notifications,
            fanout,
        }
    }

    /// Resolve once a publish on this replica has rung [`Fanout::wake`].
    pub async fn woken(&self) {
        self.fanout.wake.notified().await;
    }

    /// Drain everything claimable, and say how many rows were done.
    ///
    /// # Errors
    ///
    /// Only when the outbox cannot be read; the work itself is best-effort.
    pub async fn execute(&self) -> PortResult<usize> {
        // Batches are claimed lazily, as workers free up: the next one is claimed
        // once the previous one has been handed out, not once its slowest row has
        // finished, so one stalled notification holds one slot and not the outbox.
        let batches = futures::stream::unfold(true, |more| async move {
            if !more {
                return None;
            }
            let now = self.fanout.clock.now();
            let claimed = self
                .outbox
                .claim_fanout(
                    now,
                    OUTBOX_BATCH,
                    now + OUTBOX_LEASE,
                    MAX_ATTEMPTS,
                    now - OUTBOX_MAX_AGE,
                )
                .await;
            match claimed {
                Ok(batch) if batch.is_empty() => None,
                Ok(batch) => {
                    let more = batch.len() >= OUTBOX_BATCH as usize;
                    Some((Ok(by_notification(batch)), more))
                }
                Err(error) => Some((Err(error), false)),
            }
        });

        let (mut done, mut failure) = (0, None);
        batches
            .flat_map(|batch| {
                let items: Vec<_> = match batch {
                    Ok(groups) => groups.into_iter().map(Ok).collect(),
                    Err(error) => vec![Err(error)],
                };
                futures::stream::iter(items)
            })
            .for_each_concurrent(OUTBOX_BATCH as usize, |group| {
                let rows = match group {
                    Ok(rows) => {
                        done += rows.len();
                        Some(rows)
                    }
                    Err(error) => {
                        failure = Some(error);
                        None
                    }
                };
                async move {
                    if let Some(rows) = rows {
                        self.drain(rows).await;
                    }
                }
            })
            .await;

        failure.map_or(Ok(done), Err)
    }

    /// Work one notification's rows, holding their lease for as long as that takes
    /// (up to [`ROW_CEILING`]), and settle them: done, or given back with the
    /// reason and a backoff.
    async fn drain(&self, rows: Vec<PendingFanout>) {
        let cursors: Vec<Cursor> = rows.iter().map(|row| row.cursor).collect();
        let heartbeat = async {
            loop {
                tokio::time::sleep(LEASE_HEARTBEAT).await;
                let until = self.fanout.clock.now() + OUTBOX_LEASE;
                if let Err(error) = self.outbox.extend_fanout(&cursors, until).await {
                    tracing::warn!(?error, "fan-out lease not extended; it may be pushed twice");
                }
            }
        };
        let work = async {
            tokio::select! {
                delivered = self.deliver(&rows) => delivered,
                () = heartbeat => Ok(()),
            }
        };
        let error = match tokio::time::timeout(ROW_CEILING, work).await {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error.to_string()),
            Err(_) => Some(format!("not finished within {}s", ROW_CEILING.as_secs())),
        };

        let now = self.fanout.clock.now();
        for row in &rows {
            let settled = match &error {
                // Not completing costs a second push once the lease lapses, which a
                // client absorbs; losing the row would cost the push.
                None => self.outbox.complete_fanout(row.cursor).await,
                Some(error) => {
                    tracing::warn!(
                        cursor = %row.cursor,
                        attempts = row.attempts,
                        error,
                        "fan-out row not finished; it will be retried"
                    );
                    let retry_at = now + RETRY_STEP * row.attempts;
                    self.outbox.retry_fanout(row.cursor, error, retry_at).await
                }
            };
            if let Err(error) = settled {
                tracing::warn!(?error, "fan-out row not settled; it will repeat");
            }
        }
    }

    /// Push the notification's current state, then derive the thumbnails.
    async fn deliver(&self, rows: &[PendingFanout]) -> PortResult<()> {
        let Some(first) = rows.first() else {
            return Ok(());
        };
        // Read now rather than trusted from the write: a delete or an expiry since
        // then means nobody should be buzzed, and a later revision means the head
        // is what the push should show — whichever order the rows drain in.
        let Some(notification) = self.notifications.get(first.notification).await? else {
            return Ok(());
        };
        if notification.is_gone(self.fanout.clock.now()) {
            return Ok(());
        }

        // Skip whoever an earlier attempt at this same head already reached.
        let already = self
            .outbox
            .delivered_devices(notification.id, notification.head.seq)
            .await?;
        self.fanout
            .push_remaining(&notification, &already, Some(self.outbox.as_ref()))
            .await;

        let mut thumbnails: Vec<AttachmentId> =
            rows.iter().filter_map(|row| row.thumbnail).collect();
        thumbnails.dedup();
        for reference in thumbnails {
            self.fanout.derive_thumbnail(reference).await;
        }
        Ok(())
    }
}

/// A claimed batch, one group per notification, in claim order: two revisions of
/// one notification are one push of its head, not two pushes racing each other.
fn by_notification(batch: Vec<PendingFanout>) -> Vec<Vec<PendingFanout>> {
    let mut groups: Vec<Vec<PendingFanout>> = Vec::new();
    for row in batch {
        match groups.iter_mut().find(|group| {
            group
                .first()
                .is_some_and(|g| g.notification == row.notification)
        }) {
            Some(group) => group.push(row),
            None => groups.push(vec![row]),
        }
    }
    groups
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
    use notify_ports::NotificationRepository;
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
            panic!()
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
            panic!()
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
            panic!()
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
            panic!()
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
        let after = fanout(&store, &clock)
            .with_sinks(Arc::new(registry))
            .after_write(&notification, &revision);
        let cursor = NotificationRepository::insert_with(&store, &notification, &revision, &after)
            .await
            .unwrap();

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
            queued[0].event.cursor, cursor,
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
        let after = fanout(&store, &clock)
            .with_sinks(Arc::new(
                notify_ports::test_utils::StaticSinkRegistry::empty(),
            ))
            .after_write(&notification, &revision);
        NotificationRepository::insert_with(&store, &notification, &revision, &after)
            .await
            .unwrap();

        assert_eq!(
            notify_ports::SinkQueue::claim_due(&store, clock.now(), 10, clock.now())
                .await
                .unwrap(),
            [] as [notify_ports::SinkDelivery; 0]
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

    // --- The outbox drain (ADR-0025) ----------------------------------------

    fn drain(store: &InMemory, clock: &TestClock) -> DrainFanout {
        DrainFanout::new(
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            fanout(store, clock),
        )
    }

    /// Commit a write the way a publish does: with its outbox row.
    async fn publish(store: &InMemory, clock: &TestClock, notification: &Notification) {
        let after = fanout(store, clock).after_write(notification, &notification.head);
        NotificationRepository::insert_with(store, notification, &notification.head, &after)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn critical_a_notification_deleted_before_the_drain_is_never_pushed() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let alice = Principal::User {
            id: UserId::generate(),
        };
        seed(&store, &alice, webpush(), clock.now());
        let notification = notification(&alice, None, clock.now());
        publish(&store, &clock, &notification).await;

        NotificationRepository::mark_deleted(&store, notification.id, clock.now())
            .await
            .unwrap();
        drain(&store, &clock).execute().await.unwrap();

        assert!(
            store.pushes().is_empty(),
            "buzzing a phone about something its reader can no longer open"
        );
        assert_eq!(store.pending_fanout(), [] as [PendingFanout; 0]);
    }

    #[tokio::test]
    async fn two_revisions_drained_together_push_the_current_head_once() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let alice = Principal::User {
            id: UserId::generate(),
        };
        seed(&store, &alice, webpush(), clock.now());
        let mut notification = notification(&alice, None, clock.now());
        publish(&store, &clock, &notification).await;

        let written = notification
            .revise(
                RevisionDraft::lifecycle(State::Open, ContentDraft::titled("etcd recovered")),
                notify_domain::Author::anonymous(alice.clone()),
                clock.now(),
                &ChannelPolicy::default(),
            )
            .unwrap();
        let after = fanout(&store, &clock).after_write(&notification, &written.revision);
        NotificationRepository::append_revision_with(
            &store,
            &notification,
            &written.revision,
            &after,
        )
        .await
        .unwrap();

        assert_eq!(drain(&store, &clock).execute().await.unwrap(), 2);

        let pushes = store.pushes();
        assert_eq!(pushes.len(), 1, "one push of the head, not two racing");
        let PushMessage::Notification {
            body: PushBody::Payload { bytes, .. },
            ..
        } = &pushes[0].1
        else {
            panic!("a payload fits the budget");
        };
        assert!(
            String::from_utf8_lossy(bytes).contains("etcd recovered"),
            "the newest revision, whichever row drained first"
        );
        assert_eq!(store.pending_fanout(), [] as [PendingFanout; 0]);
    }

    #[tokio::test]
    async fn a_held_back_push_retried_is_counted_once_in_the_digest() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let alice = Principal::User {
            id: UserId::generate(),
        };
        let Principal::User { id: alice_id } = alice else {
            panic!()
        };
        // Two devices: the fan-out asks about each.
        seed(&store, &alice, webpush(), clock.now());
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

        let notification = notification(&alice, None, clock.now());
        let fanout = fanout(&store, &clock);
        // And the row is worked twice, as after a lease that ran out mid-push.
        fanout.push(&notification).await;
        fanout.push(&notification).await;

        let tallies = PreferenceRepository::deferred_tallies(&store, None, 10)
            .await
            .unwrap();
        assert_eq!(tallies.len(), 1);
        assert_eq!(tallies[0].total, 1, "one notification held back, said once");
    }

    #[tokio::test]
    async fn a_row_never_finished_is_retired_after_its_attempts_and_blocks_nothing() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let alice = Principal::User {
            id: UserId::generate(),
        };
        seed(&store, &alice, webpush(), clock.now());
        let stuck = notification(&alice, None, clock.now());
        publish(&store, &clock, &stuck).await;

        // Claimed and never settled, every lease: a worker dying on it each time.
        for _ in 0..MAX_ATTEMPTS {
            let now = clock.now();
            let claimed = store
                .claim_fanout(
                    now,
                    OUTBOX_BATCH,
                    now + OUTBOX_LEASE,
                    MAX_ATTEMPTS,
                    now - OUTBOX_MAX_AGE,
                )
                .await
                .unwrap();
            assert_eq!(claimed.len(), 1);
            clock.advance(OUTBOX_LEASE + time::Duration::seconds(1));
        }

        let healthy = notification(&alice, None, clock.now());
        publish(&store, &clock, &healthy).await;
        assert_eq!(drain(&store, &clock).execute().await.unwrap(), 1);

        assert_eq!(
            store.dead_fanout(),
            1,
            "set aside for an operator, not lost"
        );
        assert_eq!(store.pushes().len(), 1, "and the row behind it went out");
    }

    /// A relay that holds one device's push for `stall`, and lets the rest through.
    struct Stalls {
        store: InMemory,
        device: DeviceId,
        stall: std::time::Duration,
    }

    #[async_trait::async_trait]
    impl PushDispatcher for Stalls {
        async fn dispatch(
            &self,
            device: &Device,
            message: &PushMessage,
        ) -> PortResult<PushOutcome> {
            if device.id == self.device {
                tokio::time::sleep(self.stall).await;
            }
            self.store.dispatch(device, message).await
        }
    }

    fn stalling_drain(store: &InMemory, clock: &TestClock, stalls: Stalls) -> DrainFanout {
        let fanout = Fanout::new(
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(stalls),
            Arc::new(store.clone()),
            Arc::new(store.clone()),
            Arc::new(clock.clone()),
            4_096,
        );
        DrainFanout::new(Arc::new(store.clone()), Arc::new(store.clone()), fanout)
    }

    #[tokio::test(start_paused = true)]
    async fn a_row_retried_after_partial_progress_pushes_only_the_remaining_devices() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let alice = Principal::User {
            id: UserId::generate(),
        };
        let reached = seed(&store, &alice, webpush(), clock.now());
        let stalled = seed(&store, &alice, webpush(), clock.now());
        let notification = notification(&alice, None, clock.now());
        publish(&store, &clock, &notification).await;

        // One relay never answers: the row runs into its ceiling and is given back.
        let stalls = Stalls {
            store: store.clone(),
            device: stalled,
            stall: ROW_CEILING * 2,
        };
        stalling_drain(&store, &clock, stalls)
            .execute()
            .await
            .unwrap();
        assert_eq!(store.pending_fanout().len(), 1, "given back, not dropped");

        clock.advance(RETRY_STEP + time::Duration::seconds(1));
        assert_eq!(drain(&store, &clock).execute().await.unwrap(), 1);

        let pushed: Vec<DeviceId> = store.pushes().into_iter().map(|(id, _)| id).collect();
        assert_eq!(
            pushed,
            [reached, stalled],
            "the retry pushes the device still owed, not the one already reached"
        );
        assert_eq!(store.pending_fanout(), [] as [PendingFanout; 0]);
        assert_eq!(
            store
                .delivered_devices(notification.id, notification.head.seq)
                .await
                .unwrap(),
            [] as [DeviceId; 0],
            "the progress goes with the row"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_live_worker_keeps_extending_its_lease_so_nobody_else_claims_the_row() {
        let store = InMemory::new();
        let clock = TestClock::new();
        let alice = Principal::User {
            id: UserId::generate(),
        };
        let device = seed(&store, &alice, webpush(), clock.now());
        let notification = notification(&alice, None, clock.now());
        publish(&store, &clock, &notification).await;

        // Slow but alive: well past the lease the claim took.
        let stalls = Stalls {
            store: store.clone(),
            device,
            stall: std::time::Duration::from_mins(10),
        };
        let worker = stalling_drain(&store, &clock, stalls);
        let running = tokio::spawn(async move { worker.execute().await });

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        clock.advance(time::Duration::minutes(4));
        // One heartbeat: held until four minutes plus a lease from the claim.
        tokio::time::sleep(LEASE_HEARTBEAT).await;
        clock.advance(time::Duration::minutes(2));

        let now = clock.now();
        let stolen = store
            .claim_fanout(
                now,
                OUTBOX_BATCH,
                now + OUTBOX_LEASE,
                MAX_ATTEMPTS,
                now - OUTBOX_MAX_AGE,
            )
            .await
            .unwrap();
        assert_eq!(
            stolen,
            [] as [PendingFanout; 0],
            "past the claim's own lease, but the worker is still pushing"
        );

        assert_eq!(running.await.unwrap().unwrap(), 1);
        assert_eq!(store.pushes().len(), 1, "pushed once, by the one worker");
    }
}
