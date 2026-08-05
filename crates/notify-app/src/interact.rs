//! What comes back from a notification: interactions, silences, receipts.

use std::sync::Arc;

use notify_domain::{
    DomainError, Interaction, InteractionLog, InteractionRecord, NotificationId, Principal,
    Receipt, ReceiptId, ReceiptKind, Recorded, Silence, SilenceScope, SilenceTarget,
};
use notify_ports::{
    ChannelRepository, Clock, DeviceRepository, Failure, InteractionRepository, ReceiptRepository,
};

use crate::authz::Caller;
use crate::fanout::Fanout;
use crate::read::GetNotification;

/// `POST /api/v1/notifications/{id}/interactions` — ack, reply or choice
/// (design §2.3).
#[derive(Clone)]
pub struct RecordInteraction {
    interactions: Arc<dyn InteractionRepository>,
    get: GetNotification,
    clock: Arc<dyn Clock>,
    fanout: Fanout,
}

impl core::fmt::Debug for RecordInteraction {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RecordInteraction").finish_non_exhaustive()
    }
}

impl RecordInteraction {
    /// Wire it.
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

    /// Record it, applying the domain's deduplication rules.
    ///
    /// The domain decides *what* the outcome is — created, unchanged, or a pick
    /// replacing an earlier one — and the repository applies exactly that. Deciding
    /// twice, once here and once in SQL, is how the two end up disagreeing.
    pub async fn execute(
        &self,
        caller: &Caller,
        notification_id: NotificationId,
        interaction: Interaction,
    ) -> Result<Recorded, Failure> {
        // Interacting is a read-side act: a subscriber may acknowledge, and a
        // publish-only key may not.
        let notification = self.get.execute(caller, notification_id).await?;

        let existing = self.interactions.list_for(notification_id).await?;
        let mut log = InteractionLog::from_records(existing);

        let author = notify_domain::Author::anonymous(caller.principal.clone());
        let outcome = log.record(
            &notification.head.content.actions,
            notification_id,
            author,
            interaction,
            self.clock.now(),
        )?;

        if let Some(cursor) = self.interactions.apply(&outcome).await? {
            self.fanout.poke_channel(&notification, cursor).await;
        }
        Ok(outcome)
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
    /// Wire it.
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
    /// Wire it.
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
        let notification = self.get.execute(caller, notification_id).await?;

        // Silences are per user and stored per user. A service account has no
        // attention to protect, so there is nothing sensible to silence for one.
        let Principal::User { id } = caller.principal else {
            return Err(DomainError::Malformed {
                field: "silence",
                reason: "only a user can silence a notification",
            }
            .into());
        };

        let target: SilenceTarget = notify_domain::silence::target_for(
            scope,
            notification_id,
            notification.head.content.group_key.as_deref(),
        )?;
        let silence = Silence::start(id, target, duration, self.clock.now())?;

        let cursor = self.interactions.silence(&silence).await?;
        // Private to this user: their other clients update in real time, nobody
        // else's does (design §8.1, scenario T26).
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
    /// Wire it.
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

    /// Record a receipt for one of the caller's own devices.
    ///
    /// Receipts are not signed (ADR-0005): authenticity rests on the device having
    /// authenticated. What that requires of *this* layer is the check below — a
    /// principal may only post receipts for devices it owns. Without it, "unsigned"
    /// would mean "anybody can claim anybody read anything".
    pub async fn execute(
        &self,
        caller: &Caller,
        notification_id: NotificationId,
        kind: ReceiptKind,
        device: Option<notify_domain::DeviceId>,
    ) -> Result<Option<Receipt>, Failure> {
        let notification = self.get.execute(caller, notification_id).await?;

        // Ownership first, policy second. Claiming somebody else's device is an
        // attack whether or not this channel happens to collect receipts, and
        // answering it with a polite `None` would hide it.
        if let Some(device_id) = device {
            let owned = self
                .devices
                .get(device_id)
                .await?
                .is_some_and(|found| found.owner == caller.principal);
            if !owned {
                return Err(DomainError::Malformed {
                    field: "receipt.device",
                    reason: "is not a device of the authenticated principal",
                }
                .into());
            }
        }

        // Receipts are opt-in per channel (design §7.3). A channel that does not
        // collect them records nothing and says so with `None`, rather than failing
        // a client that cannot know the policy.
        let collects = self
            .channels
            .get(&notification.channel)
            .await?
            .is_some_and(|channel| channel.policy.receipts_enabled);
        if !collects {
            return Ok(None);
        }

        let receipt = Receipt {
            id: ReceiptId::generate(),
            notification_id,
            principal: caller.principal.clone(),
            device,
            kind,
            at: self.clock.now(),
        };

        if let Some(cursor) = self.receipts.record(&receipt).await? {
            self.fanout.poke_channel(&notification, cursor).await;
            return Ok(Some(receipt));
        }
        Ok(None)
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
        );

        Fixture {
            record: RecordInteraction::new(
                Arc::new(store.clone()),
                get.clone(),
                Arc::new(clock.clone()),
                fanout.clone(),
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
                get,
                Arc::new(clock.clone()),
                fanout,
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
}
