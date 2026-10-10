//! What comes back from a notification: interactions, silences, receipts.

use std::sync::Arc;

use notify_domain::{
    Author, DomainError, Interaction, InteractionLog, InteractionRecord, NotificationId, Principal,
    PushTransport, Readership, Receipt, ReceiptId, ReceiptKind, Recorded, Silence, SilenceScope,
    SilenceTarget,
};
use notify_ports::{
    ChannelRepository, Clock, DeviceRepository, Failure, InteractionRepository, PortError,
    ReceiptRepository, UserRepository, Visibility,
};

use crate::attribution::Attribution;
use crate::authz::Caller;
use crate::fanout::Fanout;
use crate::read::GetNotification;

/// How many times an interaction re-reads the log after losing a race to write it.
const RECORD_ATTEMPTS: u32 = 3;

/// `POST /api/v1/notifications/{id}/interactions` — ack, reply or choice
/// (design §2.3).
#[derive(Clone)]
pub struct RecordInteraction {
    interactions: Arc<dyn InteractionRepository>,
    get: GetNotification,
    clock: Arc<dyn Clock>,
    fanout: Fanout,
    /// Names the author (§2.4): "Alice acknowledged", not a ULID.
    attribution: Attribution,
}

impl core::fmt::Debug for RecordInteraction {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RecordInteraction").finish_non_exhaustive()
    }
}

impl RecordInteraction {
    #[must_use]
    pub fn new(
        interactions: Arc<dyn InteractionRepository>,
        get: GetNotification,
        clock: Arc<dyn Clock>,
        fanout: Fanout,
        attribution: Attribution,
    ) -> Self {
        Self {
            interactions,
            get,
            clock,
            fanout,
            attribution,
        }
    }

    /// Record it. The domain decides the outcome (created, unchanged, replaced) and the
    /// repository applies exactly that, so the two cannot disagree.
    pub async fn execute(
        &self,
        caller: &Caller,
        notification_id: NotificationId,
        interaction: Interaction,
    ) -> Result<Recorded, Failure> {
        // Interacting is a read-side act: a subscriber may acknowledge, a publish-only key may not.
        let notification = self.get.execute(caller, notification_id).await?;
        let author = self.attribution.author(&caller.principal).await;

        // Read-decide-write races (a double-clicked ack): the loser hits the unique index
        // and re-reads, so the domain decides again against the winner.
        let mut attempts = 0;
        loop {
            attempts += 1;
            let existing = self.interactions.list_for(notification_id).await?;
            let mut log = InteractionLog::from_records(existing);

            let outcome = log.record(
                &notification.head.content.actions,
                notification_id,
                author.clone(),
                interaction.clone(),
                self.clock.now(),
            )?;

            match self.interactions.apply(&outcome).await {
                Ok(Some(cursor)) => {
                    self.fanout.poke_channel(&notification, cursor).await;
                    return Ok(outcome);
                }
                Ok(None) => return Ok(outcome),
                Err(PortError::Conflict { .. }) if attempts < RECORD_ATTEMPTS => {}
                Err(other) => return Err(other.into()),
            }
        }
    }
}

/// `GET /api/v1/notifications/{id}/interactions` — what a waiting service polls.
#[derive(Clone)]
pub struct ListInteractions {
    interactions: Arc<dyn InteractionRepository>,
    get: GetNotification,
}

impl core::fmt::Debug for ListInteractions {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ListInteractions").finish_non_exhaustive()
    }
}

impl ListInteractions {
    #[must_use]
    pub fn new(interactions: Arc<dyn InteractionRepository>, get: GetNotification) -> Self {
        Self { interactions, get }
    }

    /// List them all — the script waiting on its `approve` must not have to page.
    pub async fn execute(
        &self,
        caller: &Caller,
        notification_id: NotificationId,
    ) -> Result<Vec<InteractionRecord>, Failure> {
        self.get.execute(caller, notification_id).await?;
        Ok(self.interactions.list_for(notification_id).await?)
    }
}

/// `POST /api/v1/notifications/{id}/silence` — stop notifying *me* (design §2.3).
#[derive(Clone)]
pub struct SilenceNotification {
    interactions: Arc<dyn InteractionRepository>,
    get: GetNotification,
    clock: Arc<dyn Clock>,
    fanout: Fanout,
}

impl core::fmt::Debug for SilenceNotification {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SilenceNotification")
            .finish_non_exhaustive()
    }
}

impl SilenceNotification {
    #[must_use]
    pub fn new(
        interactions: Arc<dyn InteractionRepository>,
        get: GetNotification,
        clock: Arc<dyn Clock>,
        fanout: Fanout,
    ) -> Self {
        Self {
            interactions,
            get,
            clock,
            fanout,
        }
    }

    /// Silence a notification or its whole group, for this user.
    pub async fn execute(
        &self,
        caller: &Caller,
        notification_id: NotificationId,
        scope: SilenceScope,
        duration: Option<notify_domain::HumanDuration>,
    ) -> Result<Silence, Failure> {
        // Silences are per user; a service account has no attention to protect.
        let Principal::User { id } = caller.principal else {
            return Err(DomainError::Malformed {
                field: "silence",
                reason: "only a user can silence a notification",
            }
            .into());
        };
        // A session, like do-not-disturb: a leaked publish-only key that could
        // also silence its owner would let whoever holds it mute their alerts.
        caller.ensure_person()?;

        let notification = self.get.execute(caller, notification_id).await?;

        let target: SilenceTarget = notify_domain::silence::target_for(
            scope,
            notification_id,
            notification.head.content.group_key.as_deref(),
        )?;
        let silence = Silence::start(id, target, duration, self.clock.now())?;

        let cursor = self.interactions.silence(&silence).await?;
        // Private to this user (§8.1).
        self.fanout.poke_user(id, cursor).await;
        Ok(silence)
    }
}

/// `POST /api/v1/receipts` — delivered and seen (design §7.3).
#[derive(Clone)]
pub struct PostReceipt {
    receipts: Arc<dyn ReceiptRepository>,
    devices: Arc<dyn DeviceRepository>,
    channels: Arc<dyn ChannelRepository>,
    get: GetNotification,
    clock: Arc<dyn Clock>,
    fanout: Fanout,
}

impl core::fmt::Debug for PostReceipt {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PostReceipt").finish_non_exhaustive()
    }
}

impl PostReceipt {
    #[must_use]
    pub fn new(
        receipts: Arc<dyn ReceiptRepository>,
        devices: Arc<dyn DeviceRepository>,
        channels: Arc<dyn ChannelRepository>,
        get: GetNotification,
        clock: Arc<dyn Clock>,
        fanout: Fanout,
    ) -> Self {
        Self {
            receipts,
            devices,
            channels,
            get,
            clock,
            fanout,
        }
    }

    /// Record a receipt for one of the caller's own devices. Receipts are unsigned
    /// (ADR-0005), so the ownership check below is what stops forged readings.
    pub async fn execute(
        &self,
        caller: &Caller,
        notification_id: NotificationId,
        kind: ReceiptKind,
        device: Option<notify_domain::DeviceId>,
    ) -> Result<Option<Receipt>, Failure> {
        let notification = self.get.execute(caller, notification_id).await?;

        // Ownership first: claiming another's device is an attack whatever the channel policy.
        if let Some(device_id) = device {
            let owned = self
                .devices
                .get(device_id)
                .await?
                .filter(|found| found.owner == caller.principal);
            let Some(owned) = owned else {
                return Err(DomainError::Malformed {
                    field: "receipt.device",
                    reason: "is not a device of the authenticated principal",
                }
                .into());
            };
            // The device policy (`devices::reaches`): a key manages only `stream`
            // devices, so it does not get to speak for its owner's phone either.
            if !matches!(owned.transport, PushTransport::Stream) {
                caller.ensure_session()?;
            }
        }

        // The per-channel opt-in (§7.3) governs who is shown a reading, not whether it is
        // recorded: `seen` is always stored (the unread badge depends on it) and only its
        // audience moves. `delivered` keeps the gate; no badge depends on it.
        let collects = self
            .channels
            .get(&notification.channel)
            .await?
            .is_some_and(|channel| channel.policy.receipts_enabled);

        let visibility = if collects {
            Visibility::Channel
        } else {
            if kind == ReceiptKind::Delivered {
                return Ok(None);
            }
            // Recorded, but told only to the reader's own connections.
            Visibility::Reader
        };

        let receipt = Receipt {
            id: ReceiptId::generate(),
            notification_id,
            principal: caller.principal.clone(),
            device,
            kind,
            at: self.clock.now(),
        };

        let cursor = self.receipts.record(&receipt, visibility).await?;

        match (visibility, cursor) {
            // The channel learns, as "vu par 3/5" requires.
            (Visibility::Channel, Some(cursor)) => {
                self.fanout.poke_channel(&notification, cursor).await;
                Ok(Some(receipt))
            }
            // `poke_user`, not `poke_channel`: a private read.
            (Visibility::Reader, Some(cursor)) => {
                if let Principal::User { id } = caller.principal {
                    self.fanout.poke_user(id, cursor).await;
                }
                Ok(Some(receipt))
            }
            (_, None) => Ok(None),
        }
    }
}

/// `GET /api/v1/notifications/{id}/receipts` — who read it (§7.3, §10.3). A client
/// cannot derive this from its stream, which only shows receipts while it is open.
#[derive(Clone)]
pub struct ReadReceipts {
    receipts: Arc<dyn ReceiptRepository>,
    channels: Arc<dyn ChannelRepository>,
    users: Arc<dyn UserRepository>,
    get: GetNotification,
}

impl core::fmt::Debug for ReadReceipts {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ReadReceipts").finish_non_exhaustive()
    }
}

impl ReadReceipts {
    #[must_use]
    pub fn new(
        receipts: Arc<dyn ReceiptRepository>,
        channels: Arc<dyn ChannelRepository>,
        users: Arc<dyn UserRepository>,
        get: GetNotification,
    ) -> Self {
        Self {
            receipts,
            channels,
            users,
            get,
        }
    }

    /// Read the roll, or `None` where the channel does not collect receipts: "nobody
    /// read this" and "nobody records reads here" must not render the same.
    pub async fn execute(
        &self,
        caller: &Caller,
        notification_id: NotificationId,
    ) -> Result<Option<Readership>, Failure> {
        let notification = self.get.execute(caller, notification_id).await?;

        let Some(channel) = self.channels.get(&notification.channel).await? else {
            return Ok(None);
        };
        if !channel.policy.receipts_enabled {
            return Ok(None);
        }

        // A total only where it can be counted: group access has readers this process
        // cannot know (§14.2), and service accounts never read (§5.2bis).
        let audience = channel.group_access.is_empty().then(|| {
            channel
                .members
                .iter()
                .filter(|member| member.principal.is_human())
                .count()
        });

        let mut roll = Readership::of(&self.receipts.list_for(notification_id).await?, audience);
        self.name(&mut roll).await?;
        Ok(Some(roll))
    }

    /// Attach display names to the people in the roll (§2.4); one read per person, the
    /// roll being bounded by who opened one notification.
    async fn name(&self, roll: &mut Readership) -> Result<(), Failure> {
        for sighting in &mut roll.seen {
            let Some(id) = sighting.author.principal.as_user() else {
                continue;
            };
            let Some(user) = self.users.get(id).await? else {
                // A receipt outliving its user keeps its reference.
                continue;
            };
            if let Some(name) = user.display_name.as_deref() {
                // A stored name this build refuses to render falls back to the reference.
                sighting.author = Author::named(sighting.author.principal.clone(), name)
                    .unwrap_or_else(|_| Author::anonymous(sighting.author.principal.clone()));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use notify_domain::{
        ActionDraft, ActionKey, Actions, Author, Channel, ChannelId, ChannelPolicy,
        ChoiceOptionDraft, ChoiceOptionKey, ContentDraft, Device, DeviceId, HumanDuration,
        Membership, Notification, NotificationId, PushTransport, RevisionDraft, Scope, State,
        UserId,
    };
    use notify_ports::NotificationRepository;
    use notify_ports::test_utils::{InMemory, TestClock};

    use super::*;
    use crate::authz::Authorizer;

    struct Fixture {
        store: InMemory,
        clock: TestClock,
        record: RecordInteraction,
        list: ListInteractions,
        silence: SilenceNotification,
        receipt: PostReceipt,
        readership: ReadReceipts,
        alice: Principal,
    }

    fn channel_id() -> ChannelId {
        ChannelId::parse("ci-infra").unwrap()
    }

    fn actions() -> Actions {
        Actions::new(vec![
            ActionDraft::Ack {
                label: "Ack".to_owned(),
                key: "ack-incident".to_owned(),
            }
            .try_into()
            .unwrap(),
            ActionDraft::Choice {
                label: "Escalade".to_owned(),
                key: "escalate".to_owned(),
                options: vec![
                    ChoiceOptionDraft {
                        key: "oncall".to_owned(),
                        label: "Page on-call".to_owned(),
                    },
                    ChoiceOptionDraft {
                        key: "defer".to_owned(),
                        label: "Attendre".to_owned(),
                    },
                ],
            }
            .try_into()
            .unwrap(),
        ])
        .unwrap()
    }

    fn fixture() -> Fixture {
        let store = InMemory::new();
        let clock = TestClock::new();
        let alice = Principal::User {
            id: UserId::generate(),
        };

        let mut channel = Channel::dynamic(channel_id(), None).unwrap();
        // Receipts on, so the "seen by 3/5" path is exercised rather than skipped.
        channel.policy.receipts_enabled = true;
        channel.upsert_member(
            Membership::new(alice.clone(), vec![Scope::Publish, Scope::Subscribe]).unwrap(),
        );
        store.seed_channel(channel);

        let authorizer = Authorizer::new(Arc::new(store.clone()));
        let get =
            GetNotification::new(Arc::new(store.clone()), authorizer, Arc::new(clock.clone()));
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
            record: RecordInteraction::new(
                Arc::new(store.clone()),
                get.clone(),
                Arc::new(clock.clone()),
                fanout.clone(),
                Attribution::new(Arc::new(store.clone()), Arc::new(store.clone())),
            ),
            list: ListInteractions::new(Arc::new(store.clone()), get.clone()),
            silence: SilenceNotification::new(
                Arc::new(store.clone()),
                get.clone(),
                Arc::new(clock.clone()),
                fanout.clone(),
            ),
            receipt: PostReceipt::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                get.clone(),
                Arc::new(clock.clone()),
                fanout,
            ),
            readership: ReadReceipts::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                get,
            ),
            store,
            clock,
            alice,
        }
    }

    async fn seed(fixture: &Fixture, group_key: Option<&str>) -> Notification {
        let content = ContentDraft {
            actions: actions(),
            group_key: group_key.map(str::to_owned),
            ..ContentDraft::titled("etcd degraded")
        };
        let (notification, written) = Notification::create(
            NotificationId::generate(),
            channel_id(),
            None,
            RevisionDraft::lifecycle(State::Open, content),
            Author::anonymous(fixture.alice.clone()),
            fixture.clock.now(),
            &ChannelPolicy::default(),
        )
        .unwrap();
        NotificationRepository::insert(&fixture.store, &notification, &written.revision)
            .await
            .unwrap();
        notification
    }

    fn ack() -> Interaction {
        Interaction::Ack {
            key: ActionKey::parse("ack-incident").unwrap(),
        }
    }

    // --- T4 · Interactions through the use case (design §3.4) -------------

    #[tokio::test]
    async fn critical_t4_a_second_acknowledgement_writes_and_broadcasts_nothing() {
        let fixture = fixture();
        let notification = seed(&fixture, None).await;
        let caller = Caller::member(fixture.alice.clone());

        let first = fixture
            .record
            .execute(&caller, notification.id, ack())
            .await
            .unwrap();
        assert!(matches!(first, Recorded::Created(_)));
        let pokes_after_first = fixture.store.pokes().len();

        let second = fixture
            .record
            .execute(&caller, notification.id, ack())
            .await
            .unwrap();
        assert!(matches!(second, Recorded::Unchanged(_)));
        assert_eq!(
            fixture.store.pokes().len(),
            pokes_after_first,
            "an idempotent acknowledgement is not an event"
        );
    }

    /// Loses the first write to a request that recorded the same thing a moment
    /// earlier, the way a double click races through the unique index.
    struct Racing {
        inner: InMemory,
        raced: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl InteractionRepository for Racing {
        async fn apply(
            &self,
            outcome: &Recorded,
        ) -> notify_ports::PortResult<Option<notify_domain::Cursor>> {
            if !self.raced.swap(true, std::sync::atomic::Ordering::SeqCst) {
                InteractionRepository::apply(&self.inner, outcome).await?;
                return Err(PortError::Conflict {
                    what: "interaction",
                });
            }
            InteractionRepository::apply(&self.inner, outcome).await
        }
        async fn list_for(
            &self,
            notification: NotificationId,
        ) -> notify_ports::PortResult<Vec<InteractionRecord>> {
            InteractionRepository::list_for(&self.inner, notification).await
        }
        async fn silence(
            &self,
            silence: &Silence,
        ) -> notify_ports::PortResult<notify_domain::Cursor> {
            InteractionRepository::silence(&self.inner, silence).await
        }
        async fn active_silences(
            &self,
            user: UserId,
            now: time::OffsetDateTime,
        ) -> notify_ports::PortResult<Vec<Silence>> {
            InteractionRepository::active_silences(&self.inner, user, now).await
        }
    }

    #[tokio::test]
    async fn critical_t4_an_acknowledgement_that_loses_the_race_is_unchanged_not_an_error() {
        let fixture = fixture();
        let notification = seed(&fixture, None).await;
        let caller = Caller::member(fixture.alice.clone());
        let authorizer = Authorizer::new(Arc::new(fixture.store.clone()));
        let record = RecordInteraction::new(
            Arc::new(Racing {
                inner: fixture.store.clone(),
                raced: std::sync::atomic::AtomicBool::new(false),
            }),
            GetNotification::new(
                Arc::new(fixture.store.clone()),
                authorizer,
                Arc::new(fixture.clock.clone()),
            ),
            Arc::new(fixture.clock.clone()),
            Fanout::new(
                Arc::new(fixture.store.clone()),
                Arc::new(fixture.store.clone()),
                Arc::new(fixture.store.clone()),
                Arc::new(fixture.store.clone()),
                Arc::new(fixture.store.clone()),
                Arc::new(fixture.clock.clone()),
                4_096,
            ),
            Attribution::new(
                Arc::new(fixture.store.clone()),
                Arc::new(fixture.store.clone()),
            ),
        );

        let outcome = record
            .execute(&caller, notification.id, ack())
            .await
            .unwrap();

        assert!(matches!(outcome, Recorded::Unchanged(_)));
        assert_eq!(
            fixture
                .list
                .execute(&caller, notification.id)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn critical_t4_changing_a_choice_replaces_it() {
        let fixture = fixture();
        let notification = seed(&fixture, None).await;
        let caller = Caller::member(fixture.alice.clone());

        let pick = |option: &str| Interaction::Choice {
            key: ActionKey::parse("escalate").unwrap(),
            option: ChoiceOptionKey::parse(option).unwrap(),
        };

        fixture
            .record
            .execute(&caller, notification.id, pick("defer"))
            .await
            .unwrap();
        let changed = fixture
            .record
            .execute(&caller, notification.id, pick("oncall"))
            .await
            .unwrap();

        assert!(matches!(changed, Recorded::Replaced { .. }));
        assert_eq!(
            fixture
                .list
                .execute(&caller, notification.id)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn interacting_needs_the_channel() {
        let fixture = fixture();
        let notification = seed(&fixture, None).await;
        let stranger = Caller::member(Principal::User {
            id: UserId::generate(),
        });

        assert!(
            fixture
                .record
                .execute(&stranger, notification.id, ack())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn an_interaction_on_a_button_that_no_longer_exists_says_so() {
        let fixture = fixture();
        let notification = seed(&fixture, None).await;

        let unknown = Interaction::Ack {
            key: ActionKey::parse("ack-something-else").unwrap(),
        };
        assert_eq!(
            fixture
                .record
                .execute(
                    &Caller::member(fixture.alice.clone()),
                    notification.id,
                    unknown
                )
                .await
                .unwrap_err()
                .code(),
            Some("action.unknown_key")
        );
    }

    // --- Silences ---------------------------------------------------------

    #[tokio::test]
    async fn silencing_a_group_is_private_to_the_user() {
        let fixture = fixture();
        let notification = seed(&fixture, Some("incident-4521")).await;

        let silence = fixture
            .silence
            .execute(
                &Caller::member(fixture.alice.clone()),
                notification.id,
                SilenceScope::Group,
                Some(HumanDuration::from_secs_const(4 * 3_600)),
            )
            .await
            .unwrap();

        assert!(silence.covers(notification.id, Some("incident-4521")));
        assert!(matches!(
            fixture.store.pokes().last(),
            Some(notify_ports::Poke::User { .. })
        ));
    }

    #[tokio::test]
    async fn a_group_silence_on_an_ungrouped_notification_is_refused() {
        let fixture = fixture();
        let notification = seed(&fixture, None).await;

        assert!(
            fixture
                .silence
                .execute(
                    &Caller::member(fixture.alice.clone()),
                    notification.id,
                    SilenceScope::Group,
                    Some(HumanDuration::from_secs_const(3_600)),
                )
                .await
                .is_err(),
            "a button that half-works is worse than one that says why"
        );
    }

    // --- Receipts ---------------------------------------------------------

    #[tokio::test]
    async fn a_receipt_may_only_name_a_device_the_caller_owns() {
        let fixture = fixture();
        let notification = seed(&fixture, None).await;

        let theirs = Device::enrol(
            DeviceId::generate(),
            Principal::User {
                id: UserId::generate(),
            },
            "somebody else's phone",
            PushTransport::Stream,
            None,
            fixture.clock.now(),
        )
        .unwrap();
        let theirs_id = theirs.id;
        fixture.store.seed_device(theirs);

        assert!(
            fixture
                .receipt
                .execute(
                    &Caller::member(fixture.alice.clone()),
                    notification.id,
                    ReceiptKind::Seen,
                    Some(theirs_id),
                )
                .await
                .is_err(),
            "unsigned receipts only work if you cannot post one for somebody else"
        );
    }

    #[tokio::test]
    async fn a_repeated_receipt_is_recorded_once() {
        let fixture = fixture();
        let notification = seed(&fixture, None).await;
        let caller = Caller::member(fixture.alice.clone());

        assert!(
            fixture
                .receipt
                .execute(&caller, notification.id, ReceiptKind::Seen, None)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            fixture
                .receipt
                .execute(&caller, notification.id, ReceiptKind::Seen, None)
                .await
                .unwrap()
                .is_none(),
            "\"seen by 3/5\" must not be inflatable by a retry"
        );
    }

    // --- What `receipts_enabled` governs, and what it does not -----------

    /// A second channel with the default policy: receipts off.
    async fn quiet_channel(fixture: &Fixture) -> Notification {
        let id = ChannelId::parse("cluster-ovh-1").unwrap();
        let mut channel = Channel::dynamic(id.clone(), None).unwrap();
        assert!(
            !channel.policy.receipts_enabled,
            "this test is about the default; if the default changed, it is testing nothing"
        );
        channel.upsert_member(
            Membership::new(
                fixture.alice.clone(),
                vec![Scope::Publish, Scope::Subscribe],
            )
            .unwrap(),
        );
        fixture.store.seed_channel(channel);

        let (notification, written) = Notification::create(
            NotificationId::generate(),
            id,
            None,
            RevisionDraft::lifecycle(State::Open, ContentDraft::titled("etcd degraded")),
            Author::anonymous(fixture.alice.clone()),
            fixture.clock.now(),
            &ChannelPolicy::default(),
        )
        .unwrap();
        NotificationRepository::insert(&fixture.store, &notification, &written.revision)
            .await
            .unwrap();
        notification
    }

    #[tokio::test]
    async fn a_channel_that_collects_no_receipts_still_records_that_you_read_it() {
        let fixture = fixture();
        let notification = quiet_channel(&fixture).await;
        let caller = Caller::member(fixture.alice.clone());

        fixture
            .receipt
            .execute(&caller, notification.id, ReceiptKind::Seen, None)
            .await
            .unwrap();

        // The default policy must still feed the unread badge.
        let unread = notify_ports::NotificationRepository::unread_counts(
            &fixture.store,
            &fixture.alice,
            &[ChannelId::parse("cluster-ovh-1").unwrap()],
        )
        .await
        .unwrap();

        assert!(
            unread.is_empty(),
            "a channel that collects no receipts must still know that its reader read it"
        );
    }

    #[tokio::test]
    async fn a_private_read_is_told_to_the_reader_and_to_nobody_else() {
        let fixture = fixture();
        let notification = quiet_channel(&fixture).await;
        let caller = Caller::member(fixture.alice.clone());

        fixture
            .receipt
            .execute(&caller, notification.id, ReceiptKind::Seen, None)
            .await
            .unwrap();

        // The opt-in is about who is shown the reading; broadcasting it would leak.
        let events = notify_ports::EventStore::sync(
            &fixture.store,
            fixture.alice.as_user().unwrap(),
            &notify_ports::SyncQuery {
                after: None,
                channels: Vec::new(),
                groups: Vec::new(),
                page: notify_ports::PageRequest::default(),
            },
        )
        .await
        .unwrap();

        let receipts: Vec<_> = events
            .events
            .iter()
            .filter(|event| matches!(event.payload, notify_ports::EventPayload::Receipt(_)))
            .collect();

        assert_eq!(receipts.len(), 1, "the reader's own connections must learn");
        assert!(
            receipts[0].audience.channel().is_none(),
            "a private read must not be addressed to the channel"
        );
    }

    #[tokio::test]
    async fn a_delivery_acknowledgement_keeps_the_old_gate() {
        let fixture = fixture();
        let notification = quiet_channel(&fixture).await;
        let caller = Caller::member(fixture.alice.clone());

        // `delivered` keeps the gate: no badge depends on it.
        assert!(
            fixture
                .receipt
                .execute(&caller, notification.id, ReceiptKind::Delivered, None)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_channel_that_collects_receipts_still_tells_the_channel() {
        let fixture = fixture();
        // The default fixture channel has `receipts_enabled = true`.
        let notification = seed(&fixture, None).await;
        let caller = Caller::member(fixture.alice.clone());

        fixture
            .receipt
            .execute(&caller, notification.id, ReceiptKind::Seen, None)
            .await
            .unwrap();

        let events = notify_ports::EventStore::sync(
            &fixture.store,
            fixture.alice.as_user().unwrap(),
            &notify_ports::SyncQuery {
                after: None,
                channels: Vec::new(),
                groups: Vec::new(),
                page: notify_ports::PageRequest::default(),
            },
        )
        .await
        .unwrap();

        let receipt = events
            .events
            .iter()
            .find(|event| matches!(event.payload, notify_ports::EventPayload::Receipt(_)))
            .unwrap();

        assert!(
            receipt.audience.channel().is_some(),
            "\"vu par 3/5\" is built from a channel event; moving every receipt to \
             the reader would have broken it silently"
        );
    }

    // --- The roll behind the badge (design §7.3, §10.3) -------------------

    fn known_as(fixture: &Fixture, principal: &Principal, name: &str) {
        let id = principal.as_user().unwrap();
        fixture.store.seed_user(
            notify_domain::User::from_claims(
                id,
                &format!("sub-{id}"),
                None,
                Some(name),
                fixture.clock.now(),
            )
            .unwrap(),
        );
    }

    #[tokio::test]
    async fn the_roll_says_who_read_it_and_not_which_identifier_they_are() {
        let fixture = fixture();
        let notification = seed(&fixture, None).await;
        let caller = Caller::member(fixture.alice.clone());
        known_as(&fixture, &fixture.alice, "Alice Martin");

        fixture
            .receipt
            .execute(&caller, notification.id, ReceiptKind::Seen, None)
            .await
            .unwrap();

        let roll = fixture
            .readership
            .execute(&caller, notification.id)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(roll.seen_by(), 1);
        assert_eq!(roll.audience, Some(1));
        assert_eq!(
            roll.seen[0].author.display_name.as_deref(),
            Some("Alice Martin"),
            "a hover card of raw ULIDs answers nothing (design §2.4)"
        );
    }

    #[tokio::test]
    async fn a_reader_with_no_profile_is_still_a_reader() {
        // No user row: a receipt outliving its user keeps its reference.
        let fixture = fixture();
        let notification = seed(&fixture, None).await;
        let caller = Caller::member(fixture.alice.clone());

        fixture
            .receipt
            .execute(&caller, notification.id, ReceiptKind::Seen, None)
            .await
            .unwrap();

        let roll = fixture
            .readership
            .execute(&caller, notification.id)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(roll.seen_by(), 1, "a broken link is not a missing reader");
        assert_eq!(roll.seen[0].author.display_name, None);
        assert_eq!(roll.seen[0].author.label(), fixture.alice.reference());
    }

    #[tokio::test]
    async fn a_channel_that_does_not_collect_receipts_says_so_rather_than_zero() {
        let fixture = fixture();
        let notification = seed(&fixture, None).await;
        let caller = Caller::member(fixture.alice.clone());

        let mut channel = ChannelRepository::get(&fixture.store, &channel_id())
            .await
            .unwrap()
            .unwrap();
        channel.policy.receipts_enabled = false;
        ChannelRepository::upsert(&fixture.store, &channel)
            .await
            .unwrap();

        assert!(
            fixture
                .readership
                .execute(&caller, notification.id)
                .await
                .unwrap()
                .is_none(),
            "\"nobody read it\" and \"we do not record that\" are different sentences"
        );
    }

    #[tokio::test]
    async fn the_roll_is_authorised_through_its_notification() {
        let fixture = fixture();
        let notification = seed(&fixture, None).await;
        let stranger = Caller::member(Principal::User {
            id: UserId::generate(),
        });

        assert_eq!(
            fixture
                .readership
                .execute(&stranger, notification.id)
                .await
                .unwrap_err()
                .code(),
            Some("scope.missing"),
            "who read what is channel content"
        );
    }

    #[tokio::test]
    async fn critical_t25_a_bot_that_read_it_moves_neither_half_of_the_count() {
        let fixture = fixture();
        let notification = seed(&fixture, None).await;

        let bot = Principal::ServiceAccount {
            id: notify_domain::ServiceAccountId::parse("bot-argocd").unwrap(),
        };
        let mut channel = ChannelRepository::get(&fixture.store, &channel_id())
            .await
            .unwrap()
            .unwrap();
        channel.upsert_member(Membership::new(bot.clone(), vec![Scope::Subscribe]).unwrap());
        ChannelRepository::upsert(&fixture.store, &channel)
            .await
            .unwrap();

        fixture
            .receipt
            .execute(
                &Caller::member(bot),
                notification.id,
                ReceiptKind::Seen,
                None,
            )
            .await
            .unwrap();

        let roll = fixture
            .readership
            .execute(&Caller::member(fixture.alice.clone()), notification.id)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            roll.seen_by(),
            0,
            "a bot that has read is not a human who has been informed"
        );
        assert_eq!(
            roll.audience,
            Some(1),
            "and it is not part of the total either"
        );
    }

    #[tokio::test]
    async fn a_channel_reached_through_a_group_has_no_total_to_divide_by() {
        let fixture = fixture();
        let notification = seed(&fixture, None).await;
        let caller = Caller::member(fixture.alice.clone());

        let mut channel = ChannelRepository::get(&fixture.store, &channel_id())
            .await
            .unwrap()
            .unwrap();
        channel.group_access.push(
            notify_domain::GroupBinding::new("infra-admins", vec![Scope::Subscribe]).unwrap(),
        );
        ChannelRepository::upsert(&fixture.store, &channel)
            .await
            .unwrap();

        fixture
            .receipt
            .execute(&caller, notification.id, ReceiptKind::Seen, None)
            .await
            .unwrap();

        let roll = fixture
            .readership
            .execute(&caller, notification.id)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(roll.seen_by(), 1);
        assert_eq!(
            roll.audience, None,
            "nothing here knows how many people are in `infra-admins` (§14.2), and \
             dividing by the member rows that happen to exist would read \"1/1\" on \
             a channel a whole team can see"
        );
    }
}
