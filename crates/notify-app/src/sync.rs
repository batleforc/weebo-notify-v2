//! Catching up: the offline backlog and the live stream, off one cursor.
//!
//! `GET /sync?cursor=` and `Last-Event-ID` are the same token (ADR-0001,
//! ADR-0012), so a client that was offline for a week and a client whose websocket
//! blipped take the same code path. That is the whole reason there is one event log
//! rather than a backlog table and a stream.

use std::sync::Arc;

use notify_domain::{
    ChannelId, Cursor, Device, DeviceId, DomainError, NotificationId, Principal, UserId,
};
use notify_ports::{
    Clock, DeviceRepository, EventPayload, EventStore, Failure, PageRequest, PreferenceRepository,
    SyncPage, SyncQuery,
};

use crate::authz::{Authorizer, Caller, Grant};

/// `GET /api/v1/sync` — replay what a client missed (design §8, scenario T7).
#[derive(Clone)]
pub struct SyncBacklog {
    events: Arc<dyn EventStore>,
    devices: Arc<dyn DeviceRepository>,
    preferences: Arc<dyn PreferenceRepository>,
    authorizer: Authorizer,
    clock: Arc<dyn Clock>,
}

/// What a client asks to catch up on.
#[derive(Debug, Clone, Default)]
pub struct SyncRequest {
    /// Resume after this. `None` replays everything still retained.
    pub cursor: Option<Cursor>,
    /// Narrow to these channels; empty means every channel the caller may read.
    pub channels: Vec<ChannelId>,
    /// How much.
    pub page: PageRequest,
    /// Which device is catching up, so its cursor can be advanced. Absent for a
    /// one-off caller — `curl /sync` from a shell has no cursor to keep.
    pub device: Option<DeviceId>,
}

impl core::fmt::Debug for SyncBacklog {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SyncBacklog").finish_non_exhaustive()
    }
}

impl SyncBacklog {
    /// Wire it.
    #[must_use]
    pub fn new(
        events: Arc<dyn EventStore>,
        devices: Arc<dyn DeviceRepository>,
        preferences: Arc<dyn PreferenceRepository>,
        authorizer: Authorizer,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            events,
            devices,
            preferences,
            authorizer,
            clock,
        }
    }

    /// Replay.
    ///
    /// Only users sync: the backlog includes `preference` events, which are private
    /// to one user, and a cursor belongs to a device that belongs to somebody. A
    /// service account holding a logical subscription reads its channels through the
    /// inbox instead.
    pub async fn execute(
        &self,
        caller: &Caller,
        request: SyncRequest,
    ) -> Result<SyncPage, Failure> {
        let Principal::User { id: user } = caller.principal else {
            return Err(DomainError::ScopeMissing { scope: "subscribe" }.into());
        };

        // Refreshed on every sync/stream session — a stream connection resumes its
        // backlog through this same call (`http-actix::routes::stream::Connection::
        // drain`) — not only at enrolment: it bounds how stale a group-derived push
        // target can get to "since this principal was last active" rather than
        // "since they last enrolled a device" (§11).
        //
        // Only for a session that carries groups at all: an API key's are empty by
        // construction (`auth.rs`), and writing that down would erase the snapshot
        // the owner's last real login left behind.
        if caller.grant == Grant::Membership {
            self.devices
                .remember_groups(&caller.principal, &caller.groups, self.clock.now())
                .await?;
        }

        // Only a device the caller owns has its cursor read or moved. A stranger's
        // device id is treated as no device at all — same answer as `RevokeDevice`.
        let device = match request.device {
            Some(id) => owned_device(self.devices.as_ref(), caller, id).await?,
            None => None,
        };
        // The cursor is device-wide, so only a sync that saw every channel the owner
        // can read may move it: a narrowed one (a filter, or a key's own channel list)
        // would skip whatever it did not ask for.
        let complete = request.channels.is_empty() && caller.grant == Grant::Membership;

        let readable = self.authorizer.readable_channels(caller).await?;
        let channels = if request.channels.is_empty() {
            readable
        } else {
            // Narrows, never widens.
            request
                .channels
                .into_iter()
                .filter(|channel| readable.contains(channel))
                .collect()
        };

        let mut page = self
            .events
            .sync(
                user,
                &SyncQuery {
                    after: request.cursor,
                    channels,
                    groups: caller.groups.clone(),
                    page: request.page,
                },
            )
            .await?;

        page.deferred_by_dnd = self.deferred(user, &page).await?;

        // Advance the device to the furthest event it actually received. Doing it
        // from the events rather than from `page.next` is what keeps a short page
        // from skipping anything: the device only ever claims what it was handed.
        if let (true, Some(device), Some(furthest)) = (
            complete,
            device,
            page.events.iter().map(|event| event.cursor).max(),
        ) {
            self.devices
                .advance_cursor(device.id, furthest, self.clock.now())
                .await?;
        }

        Ok(page)
    }

    /// Which notifications on this page arrived late because their owner asked for
    /// quiet (design §7.4).
    ///
    /// # It is derived, not recorded
    ///
    /// The alternative is a row per suppressed push, written on the fan-out path and
    /// pruned forever after — a table that grows with every deferred notification,
    /// to answer a question about a badge. This asks the preference instead: *was a
    /// quiet period running for this channel when the notification was created?*
    ///
    /// The cost is honest and worth stating: the preference read is the **current**
    /// one, so somebody who rewrites their quiet hours changes the answer for
    /// notifications already in their backlog. That is a badge appearing or
    /// disappearing on a row nobody is looking at any more, and it buys the absence
    /// of a growing table on the hottest path in the product.
    ///
    /// Priority is taken from the revision the event carries rather than assumed: a
    /// user who lets `urgent` through was never deferred by one, so flagging it would
    /// hide a badge that is telling the truth.
    async fn deferred(
        &self,
        user: UserId,
        page: &SyncPage,
    ) -> Result<Vec<NotificationId>, Failure> {
        // Notifications, not events: forty revisions of one deploy are one row to a
        // client, and one badge.
        let mut deferred = Vec::new();
        if page.events.is_empty() {
            return Ok(deferred);
        }

        let dnd = self.preferences.dnd(user).await?;
        // Nothing set means nothing was ever deferred, and no reason to walk the
        // page — the common case by a wide margin.
        if dnd.quiet_hours.is_empty() && dnd.until.is_none() {
            return Ok(deferred);
        }

        for event in &page.events {
            let (EventPayload::Revision(revision), Some(channel)) =
                (&event.payload, event.audience.channel())
            else {
                continue;
            };
            if deferred.contains(&revision.notification_id) {
                continue;
            }
            if dnd.is_muted(channel, revision.priority, revision.created_at) {
                deferred.push(revision.notification_id);
            }
        }

        Ok(deferred)
    }
}

/// Where a live stream should start.
///
/// A client that has never synced starts at the *head*, not at the beginning of
/// retention: opening the web app for the first time should not replay a month of
/// somebody else's incidents into a toast queue.
#[derive(Clone)]
pub struct ResolveStreamStart {
    events: Arc<dyn EventStore>,
    devices: Arc<dyn DeviceRepository>,
}

impl core::fmt::Debug for ResolveStreamStart {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ResolveStreamStart").finish_non_exhaustive()
    }
}

impl ResolveStreamStart {
    /// Wire it.
    #[must_use]
    pub fn new(events: Arc<dyn EventStore>, devices: Arc<dyn DeviceRepository>) -> Self {
        Self { events, devices }
    }

    /// Resolve, in order of precedence: an explicit `Last-Event-ID`, then the
    /// device's stored cursor, then the head of the log.
    pub async fn execute(
        &self,
        caller: &Caller,
        last_event_id: Option<Cursor>,
        device: Option<DeviceId>,
    ) -> Result<Cursor, Failure> {
        if let Some(cursor) = last_event_id {
            return Ok(cursor);
        }
        if let Some(device) = device
            && let Some(found) = owned_device(self.devices.as_ref(), caller, device).await?
            && let Some(cursor) = found.cursor
        {
            return Ok(cursor);
        }
        Ok(self.events.head().await?)
    }
}

/// The device, if it exists and belongs to the caller.
async fn owned_device(
    devices: &dyn DeviceRepository,
    caller: &Caller,
    id: DeviceId,
) -> Result<Option<Device>, Failure> {
    Ok(devices
        .get(id)
        .await?
        .filter(|device| device.owner == caller.principal))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use notify_domain::{
        Author, Channel, ChannelPolicy, ContentDraft, Device, Dnd, Membership, Notification,
        NotificationId, PushTransport, RevisionDraft, Scope, State, UserId,
    };
    use notify_ports::NotificationRepository;
    use notify_ports::test_utils::{InMemory, TestClock};

    use super::*;
    use crate::authz::Authorizer;

    struct Fixture {
        store: InMemory,
        clock: TestClock,
        sync: SyncBacklog,
        start: ResolveStreamStart,
        alice: Principal,
    }

    fn channel_id(id: &str) -> ChannelId {
        ChannelId::parse(id).unwrap()
    }

    fn fixture() -> Fixture {
        let store = InMemory::new();
        let clock = TestClock::new();
        let alice = Principal::User {
            id: UserId::generate(),
        };

        let mut channel = Channel::dynamic(channel_id("ci-infra"), None).unwrap();
        channel.upsert_member(
            Membership::new(alice.clone(), vec![Scope::Publish, Scope::Subscribe]).unwrap(),
        );
        store.seed_channel(channel);

        Fixture {
            sync: SyncBacklog::new(
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Arc::new(store.clone()),
                Authorizer::new(Arc::new(store.clone())),
                Arc::new(clock.clone()),
            ),
            start: ResolveStreamStart::new(Arc::new(store.clone()), Arc::new(store.clone())),
            store,
            clock,
            alice,
        }
    }

    async fn publish(fixture: &Fixture, title: &str) -> Notification {
        let (notification, written) = Notification::create(
            NotificationId::generate(),
            channel_id("ci-infra"),
            None,
            RevisionDraft::lifecycle(State::Open, ContentDraft::titled(title)),
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

    fn device(fixture: &Fixture) -> DeviceId {
        let device = Device::enrol(
            DeviceId::generate(),
            fixture.alice.clone(),
            "laptop",
            PushTransport::Stream,
            None,
            fixture.clock.now(),
        )
        .unwrap();
        let id = device.id;
        fixture.store.seed_device(device);
        id
    }

    // --- T10 · Lateness that was asked for (design §3.4, §7.4) ------------

    async fn set_dnd(fixture: &Fixture, dnd: Dnd) {
        PreferenceRepository::set_dnd(&fixture.store, &dnd, fixture.clock.now())
            .await
            .unwrap();
    }

    fn alice_id(fixture: &Fixture) -> UserId {
        let Principal::User { id } = fixture.alice else {
            unreachable!()
        };
        id
    }

    async fn caught_up(fixture: &Fixture) -> SyncPage {
        fixture
            .sync
            .execute(
                &Caller::member(fixture.alice.clone()),
                SyncRequest::default(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn critical_t10_a_notification_created_during_a_quiet_period_is_flagged_as_deferred() {
        let fixture = fixture();
        set_dnd(
            &fixture,
            Dnd {
                until: Some(fixture.clock.now() + time::Duration::hours(8)),
                ..Dnd::off(alice_id(&fixture))
            },
        )
        .await;
        let published = publish(&fixture, "etcd degraded").await;

        let page = caught_up(&fixture).await;

        assert_eq!(
            page.deferred_by_dnd,
            vec![published.id],
            "this lateness was asked for, so the client must not badge it (§7.2, §7.4)"
        );
    }

    #[tokio::test]
    async fn critical_t10_the_page_and_its_events_agree_about_a_quiet_period() {
        // D22: the answer to "should this make a noise" is computed here, once, per
        // reader — and the transports carry it on each event so no client has to
        // re-derive the rule. The two halves must never disagree: a page that lists
        // a notification as deferred while its own event says otherwise would be
        // worse than either alone.
        let fixture = fixture();
        set_dnd(
            &fixture,
            Dnd {
                until: Some(fixture.clock.now() + time::Duration::hours(8)),
                ..Dnd::off(alice_id(&fixture))
            },
        )
        .await;
        let published = publish(&fixture, "etcd degraded").await;

        let page = caught_up(&fixture).await;

        assert_eq!(page.deferred_by_dnd, vec![published.id]);
        // The flag itself is set by the transports, which is where the reader is
        // known; what this pins is the list they read it from, on the same page.
        let revision = page
            .events
            .iter()
            .find(|event| matches!(&event.payload, EventPayload::Revision(_)))
            .unwrap(); // the publish appended a revision event
        let EventPayload::Revision(revision) = &revision.payload else {
            unreachable!()
        };
        assert!(
            page.deferred_by_dnd.contains(&revision.notification_id),
            "the event a client renders and the list it badges from name the same \n             notification"
        );
    }

    #[tokio::test]
    async fn critical_t10_a_notification_outside_any_quiet_period_is_not_flagged() {
        let fixture = fixture();
        // A preference that exists but is not running: having one is not the same
        // as being in one.
        set_dnd(
            &fixture,
            Dnd {
                until: Some(fixture.clock.now() - time::Duration::hours(1)),
                ..Dnd::off(alice_id(&fixture))
            },
        )
        .await;
        publish(&fixture, "etcd degraded").await;

        assert!(caught_up(&fixture).await.deferred_by_dnd.is_empty());
    }

    #[tokio::test]
    async fn a_channel_that_rings_anyway_is_never_reported_as_deferred() {
        let fixture = fixture();
        set_dnd(
            &fixture,
            Dnd {
                until: Some(fixture.clock.now() + time::Duration::hours(8)),
                exceptions: vec![channel_id("ci-infra")],
                ..Dnd::off(alice_id(&fixture))
            },
        )
        .await;
        publish(&fixture, "etcd degraded").await;

        assert!(
            caught_up(&fixture).await.deferred_by_dnd.is_empty(),
            "it was never held back, so its lateness is real and worth a badge"
        );
    }

    #[tokio::test]
    async fn somebody_with_no_preference_pays_nothing_for_the_flag() {
        let fixture = fixture();
        publish(&fixture, "etcd degraded").await;

        assert!(caught_up(&fixture).await.deferred_by_dnd.is_empty());
    }

    // --- T7 · Resuming (design §3.4) --------------------------------------

    #[tokio::test]
    async fn critical_t7_syncing_advances_the_device_to_what_it_received() {
        let fixture = fixture();
        let device = device(&fixture);
        for index in 0..3 {
            publish(&fixture, &format!("n{index}")).await;
        }

        let page = fixture
            .sync
            .execute(
                &Caller::member(fixture.alice.clone()),
                SyncRequest {
                    device: Some(device),
                    ..SyncRequest::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(page.events.len(), 3);
        let stored = DeviceRepository::get(&fixture.store, device)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.cursor,
            page.events.iter().map(|event| event.cursor).max(),
            "a device only ever claims what it was actually handed"
        );
    }

    #[tokio::test]
    async fn critical_t7_a_short_page_does_not_skip_the_rest() {
        let fixture = fixture();
        let device = device(&fixture);
        for index in 0..5 {
            publish(&fixture, &format!("n{index}")).await;
        }
        let caller = Caller::member(fixture.alice.clone());

        let first = fixture
            .sync
            .execute(
                &caller,
                SyncRequest {
                    page: PageRequest::of_size(2),
                    device: Some(device),
                    ..SyncRequest::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(first.events.len(), 2);

        let resumed = fixture
            .sync
            .execute(
                &caller,
                SyncRequest {
                    cursor: first.events.iter().map(|event| event.cursor).max(),
                    page: PageRequest::of_size(10),
                    device: Some(device),
                    ..SyncRequest::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(resumed.events.len(), 3, "no gap, no repeat");
    }

    #[tokio::test]
    async fn syncing_remembers_the_callers_groups_for_group_bound_push_targets() {
        let fixture = fixture();
        let mut channel = Channel::dynamic(channel_id("cluster-ovh-1"), None).unwrap();
        channel.upsert_group(
            notify_domain::GroupBinding::new("infra-admins", vec![Scope::Subscribe]).unwrap(),
        );
        fixture.store.seed_channel(channel);

        let device = Device {
            id: DeviceId::generate(),
            owner: fixture.alice.clone(),
            label: "laptop".to_owned(),
            transport: PushTransport::Stream,
            public_key: None,
            cursor: None,
            enrolled_at: fixture.clock.now(),
            last_seen_at: None,
            revoked_at: None,
        };
        fixture.store.seed_device(device);

        assert!(
            DeviceRepository::delivery_targets(&fixture.store, &channel_id("cluster-ovh-1"))
                .await
                .unwrap()
                .is_empty(),
            "the device exists but nothing yet ties alice to the group"
        );

        fixture
            .sync
            .execute(
                &Caller::member_of(fixture.alice.clone(), vec!["infra-admins".to_owned()]),
                SyncRequest::default(),
            )
            .await
            .unwrap();

        assert_eq!(
            DeviceRepository::delivery_targets(&fixture.store, &channel_id("cluster-ovh-1"))
                .await
                .unwrap()
                .len(),
            1,
            "syncing should have remembered alice's groups, same as enrolling a device does"
        );
    }

    #[tokio::test]
    async fn syncing_narrows_to_what_the_caller_may_read() {
        let fixture = fixture();
        let bob = Principal::User {
            id: UserId::generate(),
        };
        let mut theirs = Channel::dynamic(channel_id("cluster-ovh-1"), None).unwrap();
        theirs.upsert_member(Membership::new(bob, vec![Scope::Subscribe]).unwrap());
        fixture.store.seed_channel(theirs);

        let page = fixture
            .sync
            .execute(
                &Caller::member(fixture.alice.clone()),
                SyncRequest {
                    channels: vec![channel_id("cluster-ovh-1")],
                    ..SyncRequest::default()
                },
            )
            .await
            .unwrap();
        assert!(page.events.is_empty());
    }

    #[tokio::test]
    async fn a_service_account_does_not_sync() {
        let fixture = fixture();
        let bot = Caller::member(Principal::ServiceAccount {
            id: notify_domain::ServiceAccountId::parse("bot-argocd").unwrap(),
        });

        assert!(
            fixture
                .sync
                .execute(&bot, SyncRequest::default())
                .await
                .is_err(),
            "the backlog carries private preference events; a bot has none"
        );
    }

    #[tokio::test]
    async fn a_first_time_client_starts_at_the_head() {
        let fixture = fixture();
        publish(&fixture, "before you arrived").await;

        let start = fixture
            .start
            .execute(&Caller::member(fixture.alice.clone()), None, None)
            .await
            .unwrap();
        assert_eq!(
            start,
            EventStore::head(&fixture.store).await.unwrap(),
            "opening the app must not replay a month into a toast queue"
        );
    }

    #[tokio::test]
    async fn an_explicit_last_event_id_beats_the_stored_cursor() {
        let fixture = fixture();
        let device = device(&fixture);
        let caller = Caller::member(fixture.alice.clone());
        DeviceRepository::advance_cursor(
            &fixture.store,
            device,
            Cursor::from_sequence(5),
            fixture.clock.now(),
        )
        .await
        .unwrap();

        assert_eq!(
            fixture
                .start
                .execute(&caller, Some(Cursor::from_sequence(2)), Some(device))
                .await
                .unwrap(),
            Cursor::from_sequence(2)
        );
        assert_eq!(
            fixture
                .start
                .execute(&caller, None, Some(device))
                .await
                .unwrap(),
            Cursor::from_sequence(5)
        );
    }

    #[tokio::test]
    async fn a_key_sync_leaves_the_owners_group_snapshot_alone() {
        let fixture = fixture();
        let groups = vec!["infra-admins".to_owned()];
        DeviceRepository::remember_groups(
            &fixture.store,
            &fixture.alice,
            &groups,
            fixture.clock.now(),
        )
        .await
        .unwrap();
        let as_key = Caller {
            principal: fixture.alice.clone(),
            grant: crate::authz::Grant::ApiKey {
                id: notify_domain::ApiKeyId::generate(),
                scopes: vec![Scope::Subscribe],
                channels: vec![channel_id("ci-infra")],
                publish_per_minute: None,
                owner_groups: groups.clone(),
            },
            groups: Vec::new(),
            platform_admin: false,
            expires_at: None,
        };

        fixture
            .sync
            .execute(&as_key, SyncRequest::default())
            .await
            .unwrap();

        assert_eq!(
            DeviceRepository::remembered_groups(
                &fixture.store,
                &fixture.alice,
                fixture.clock.now()
            )
            .await
            .unwrap(),
            groups,
            "a key carries no groups; writing that down would cut group-bound push"
        );
    }

    #[tokio::test]
    async fn a_strangers_device_cursor_is_neither_read_nor_moved() {
        let fixture = fixture();
        let device = device(&fixture);
        publish(&fixture, "for alice").await;
        publish(&fixture, "and again").await;
        let mallory = Caller::member(Principal::User {
            id: UserId::generate(),
        });

        fixture
            .sync
            .execute(
                &mallory,
                SyncRequest {
                    device: Some(device),
                    ..SyncRequest::default()
                },
            )
            .await
            .unwrap();
        let stored = DeviceRepository::get(&fixture.store, device)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.cursor, None, "alice's backlog must not be skipped");

        DeviceRepository::advance_cursor(
            &fixture.store,
            device,
            Cursor::from_sequence(1),
            fixture.clock.now(),
        )
        .await
        .unwrap();
        assert_eq!(
            fixture
                .start
                .execute(&mallory, None, Some(device))
                .await
                .unwrap(),
            EventStore::head(&fixture.store).await.unwrap(),
            "a stranger's device id is no device at all"
        );
    }

    #[tokio::test]
    async fn a_filtered_sync_does_not_move_the_device_cursor() {
        let fixture = fixture();
        let device = device(&fixture);
        publish(&fixture, "n").await;

        fixture
            .sync
            .execute(
                &Caller::member(fixture.alice.clone()),
                SyncRequest {
                    channels: vec![channel_id("ci-infra")],
                    device: Some(device),
                    ..SyncRequest::default()
                },
            )
            .await
            .unwrap();
        let stored = DeviceRepository::get(&fixture.store, device)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.cursor, None,
            "channels it did not ask for would otherwise be skipped"
        );
    }
}
