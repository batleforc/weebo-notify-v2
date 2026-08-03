//! Catching up: the offline backlog and the live stream, off one cursor.
//!
//! `GET /sync?cursor=` and `Last-Event-ID` are the same token (ADR-0001,
//! ADR-0012), so a client that was offline for a week and a client whose websocket
//! blipped take the same code path. That is the whole reason there is one event log
//! rather than a backlog table and a stream.

use std::sync::Arc;

use notify_domain::{ChannelId, Cursor, DeviceId, DomainError, Principal};
use notify_ports::{
    Clock, DeviceRepository, EventStore, Failure, PageRequest, SyncPage, SyncQuery,
};

use crate::authz::{Authorizer, Caller};

/// `GET /api/v1/sync` — replay what a client missed (design §8, scenario T7).
#[derive(Clone)]
pub struct SyncBacklog {
    events: Arc<dyn EventStore>,
    devices: Arc<dyn DeviceRepository>,
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
        authorizer: Authorizer,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            events,
            devices,
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

        let page = self
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

        // Advance the device to the furthest event it actually received. Doing it
        // from the events rather than from `page.next` is what keeps a short page
        // from skipping anything: the device only ever claims what it was handed.
        if let (Some(device), Some(furthest)) = (
            request.device,
            page.events.iter().map(|event| event.cursor).max(),
        ) {
            self.devices
                .advance_cursor(device, furthest, self.clock.now())
                .await?;
        }

        Ok(page)
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
        last_event_id: Option<Cursor>,
        device: Option<DeviceId>,
    ) -> Result<Cursor, Failure> {
        if let Some(cursor) = last_event_id {
            return Ok(cursor);
        }
        if let Some(device) = device
            && let Some(found) = self.devices.get(device).await?
            && let Some(cursor) = found.cursor
        {
            return Ok(cursor);
        }
        Ok(self.events.head().await?)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

    use notify_domain::{
        Author, Channel, ChannelPolicy, ContentDraft, Device, Membership, Notification,
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

        let start = fixture.start.execute(None, None).await.unwrap();
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
                .execute(Some(Cursor::from_sequence(2)), Some(device))
                .await
                .unwrap(),
            Cursor::from_sequence(2)
        );
        assert_eq!(
            fixture.start.execute(None, Some(device)).await.unwrap(),
            Cursor::from_sequence(5)
        );
    }
}
