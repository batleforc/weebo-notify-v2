//! Reading notifications: the inbox, one notification, its timeline, and search.
//! Every read is scoped through [`Authorizer::readable_channels`]; no channels is an empty page.

use std::sync::Arc;

use notify_domain::{
    ChannelId, DomainError, Notification, NotificationId, Priority, Revision, Scope, State, Status,
};
use notify_ports::{
    Allowance, Clock, Failure, InboxQuery, NotificationRepository, Page, PageRequest, Quota,
    RateLimiter, SearchQuery, UnreadCount,
};

use crate::authz::{Authorizer, Caller};

/// `GET /api/v1/unread` — the badge (§7.3, §9.2, §10.4). Its own route because it
/// changes on every publish and read, unlike the channel list. Unread: readable, live,
/// no `seen` receipt from the caller (`seen` is recorded on every channel).
#[derive(Clone)]
pub struct CountUnread {
    notifications: Arc<dyn NotificationRepository>,
    authorizer: Authorizer,
}

impl core::fmt::Debug for CountUnread {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CountUnread").finish_non_exhaustive()
    }
}

impl CountUnread {
    #[must_use]
    pub fn new(notifications: Arc<dyn NotificationRepository>, authorizer: Authorizer) -> Self {
        Self {
            notifications,
            authorizer,
        }
    }

    /// Count, over every channel the caller may read.
    pub async fn execute(&self, caller: &Caller) -> Result<Vec<UnreadCount>, Failure> {
        let readable = self.authorizer.readable_channels(caller).await?;
        Ok(self
            .notifications
            .unread_counts(&caller.principal, &readable)
            .await?)
    }
}

/// `GET /api/v1/channels/{id}/notifications` — the inbox (design §8).
#[derive(Clone)]
pub struct ListInbox {
    notifications: Arc<dyn NotificationRepository>,
    authorizer: Authorizer,
}

/// Inbox filters as a caller supplies them.
#[derive(Debug, Clone, Default)]
pub struct InboxFilters {
    /// Restrict to these channels; empty means every channel the caller may read.
    pub channels: Vec<ChannelId>,
    /// Lifecycle states to keep.
    pub states: Vec<State>,
    /// Statuses to keep.
    pub statuses: Vec<Status>,
    /// Tags that must all be present.
    pub tags: Vec<String>,
    /// Floor on priority.
    pub min_priority: Option<Priority>,
    /// Restrict to one client-side group.
    pub group_key: Option<String>,
    /// Keep only notifications carrying at least one action button.
    pub has_actions: Option<bool>,
    /// Where and how much.
    pub page: PageRequest,
}

impl core::fmt::Debug for ListInbox {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ListInbox").finish_non_exhaustive()
    }
}

impl ListInbox {
    #[must_use]
    pub fn new(notifications: Arc<dyn NotificationRepository>, authorizer: Authorizer) -> Self {
        Self {
            notifications,
            authorizer,
        }
    }

    pub async fn execute(
        &self,
        caller: &Caller,
        filters: InboxFilters,
    ) -> Result<Page<Notification>, Failure> {
        let readable = self.authorizer.readable_channels(caller).await?;

        // An explicit channel list narrows what the caller may see, never widens it.
        let channels = if filters.channels.is_empty() {
            readable
        } else {
            filters
                .channels
                .into_iter()
                .filter(|channel| readable.contains(channel))
                .collect()
        };

        if channels.is_empty() {
            return Ok(Page::empty());
        }

        Ok(self
            .notifications
            .list(&InboxQuery {
                channels,
                states: filters.states,
                statuses: filters.statuses,
                tags: filters.tags,
                min_priority: filters.min_priority,
                group_key: filters.group_key,
                has_actions: filters.has_actions,
                page: filters.page,
            })
            .await?)
    }
}

/// `GET /api/v1/notifications/{id}` — one notification (design §8).
#[derive(Clone)]
pub struct GetNotification {
    notifications: Arc<dyn NotificationRepository>,
    authorizer: Authorizer,
    clock: Arc<dyn Clock>,
}

impl core::fmt::Debug for GetNotification {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GetNotification").finish_non_exhaustive()
    }
}

impl GetNotification {
    #[must_use]
    pub fn new(
        notifications: Arc<dyn NotificationRepository>,
        authorizer: Authorizer,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            notifications,
            authorizer,
            clock,
        }
    }

    /// Fetch, checking the caller may read its channel. An id with no row at all is
    /// `notification.gone` before any channel check, so a stranger holding an id can tell
    /// "purged or never existed" (`409`) from "exists elsewhere" (`403`). Accepted: ids
    /// are unguessable ULIDs, and a reader of a purged notification must see `gone`.
    pub async fn execute(
        &self,
        caller: &Caller,
        id: NotificationId,
    ) -> Result<Notification, Failure> {
        let found = self
            .notifications
            .get(id)
            .await?
            .ok_or(DomainError::NotificationGone)?;

        self.authorizer
            .authorize(caller, &found.channel, Scope::Subscribe)
            .await?;

        if found.is_gone(self.clock.now()) {
            return Err(DomainError::NotificationGone.into());
        }
        Ok(found)
    }
}

/// The revision history of one notification (design §8).
#[derive(Clone)]
pub struct ListRevisions {
    notifications: Arc<dyn NotificationRepository>,
    get: GetNotification,
}

impl core::fmt::Debug for ListRevisions {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ListRevisions").finish_non_exhaustive()
    }
}

impl ListRevisions {
    #[must_use]
    pub fn new(notifications: Arc<dyn NotificationRepository>, get: GetNotification) -> Self {
        Self { notifications, get }
    }

    /// List, oldest first.
    pub async fn execute(
        &self,
        caller: &Caller,
        id: NotificationId,
        page: PageRequest,
    ) -> Result<Page<Revision>, Failure> {
        // Authorised through the notification itself.
        self.get.execute(caller, id).await?;
        Ok(self.notifications.revisions(id, page).await?)
    }
}

/// `GET /api/v1/search` (design §8, scenario T27).
#[derive(Clone)]
pub struct SearchNotifications {
    notifications: Arc<dyn NotificationRepository>,
    authorizer: Authorizer,
    limiter: Arc<dyn RateLimiter>,
    clock: Arc<dyn Clock>,
}

impl core::fmt::Debug for SearchNotifications {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SearchNotifications")
            .finish_non_exhaustive()
    }
}

impl SearchNotifications {
    #[must_use]
    pub fn new(
        notifications: Arc<dyn NotificationRepository>,
        authorizer: Authorizer,
        limiter: Arc<dyn RateLimiter>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            notifications,
            authorizer,
            limiter,
            clock,
        }
    }

    /// Search within the caller's channels.
    pub async fn execute(
        &self,
        caller: &Caller,
        text: &str,
        page: PageRequest,
    ) -> Result<Page<Notification>, Failure> {
        // Full text is the expensive read (§8.2).
        let quota = Quota::Search {
            principal: caller.principal.reference(),
        };
        if let Allowance::Limited {
            retry_after_seconds,
            // No channel override: a search crosses every readable channel.
        } = self.limiter.check(&quota, None, self.clock.now()).await?
        {
            return Err(Failure::RateLimited {
                retry_after_seconds,
            });
        }

        let text = text.trim();
        if text.is_empty() {
            return Ok(Page::empty());
        }

        let channels = self.authorizer.readable_channels(caller).await?;
        if channels.is_empty() {
            return Ok(Page::empty());
        }

        Ok(self
            .notifications
            .search(&SearchQuery {
                text: text.to_owned(),
                channels,
                page,
            })
            .await?)
    }
}

/// `POST /api/v1/notifications/{id}/close` — a person ends a live notification
/// (RFC 0004): no revision, a `closure` event naming them, and a silent push that
/// replaces the alert on browsers nobody is looking at.
#[derive(Clone)]
pub struct CloseNotification {
    notifications: Arc<dyn NotificationRepository>,
    authorizer: Authorizer,
    clock: Arc<dyn Clock>,
    fanout: crate::fanout::Fanout,
    attribution: crate::attribution::Attribution,
    metrics: Arc<dyn notify_ports::Metrics>,
}

impl core::fmt::Debug for CloseNotification {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CloseNotification").finish_non_exhaustive()
    }
}

impl CloseNotification {
    #[must_use]
    pub fn new(
        notifications: Arc<dyn NotificationRepository>,
        authorizer: Authorizer,
        clock: Arc<dyn Clock>,
        fanout: crate::fanout::Fanout,
        attribution: crate::attribution::Attribution,
        metrics: Arc<dyn notify_ports::Metrics>,
    ) -> Self {
        Self {
            notifications,
            authorizer,
            clock,
            fanout,
            attribution,
            metrics,
        }
    }

    /// Close, needing `publish` or `manage` in the notification's own channel. An
    /// unknown id answers as a forbidden one, so ids cannot be probed. Idempotent: a
    /// closed notification is returned as it stands and nothing is written.
    pub async fn execute(
        &self,
        caller: &Caller,
        id: NotificationId,
    ) -> Result<Notification, Failure> {
        let forbidden = || -> Failure { DomainError::ScopeMissing { scope: "publish" }.into() };
        let Some(found) = self.notifications.get(id).await? else {
            return Err(forbidden());
        };
        // Closing is weaker than deleting, which `publish` grants; `manage` gains it so
        // a channel's administrator can end what was left open.
        // Only a refusal falls through to `manage` and becomes the one `403`; the
        // store failing stays a `5xx`, and is logged by the transport.
        match self
            .authorizer
            .authorize(caller, &found.channel, Scope::Publish)
            .await
        {
            Ok(_) => {}
            Err(Failure::Domain(_)) => {
                match self
                    .authorizer
                    .authorize(caller, &found.channel, Scope::Manage)
                    .await
                {
                    Ok(_) => {}
                    Err(Failure::Domain(_)) => return Err(forbidden()),
                    Err(failure) => return Err(failure),
                }
            }
            Err(failure) => return Err(failure),
        }
        let now = self.clock.now();
        if found.is_gone(now) {
            return Err(DomainError::NotificationGone.into());
        }

        let author = self.attribution.author(&caller.principal).await;
        let Some(cursor) = self.notifications.close(id, now, &author).await? else {
            // Already closed, by now or by a concurrent writer: as it stands.
            return Ok(self.notifications.get(id).await?.unwrap_or(found));
        };
        tracing::debug!(notification = %id, principal = %caller.principal.reference(), "closed by hand");
        self.metrics.notification_closed("manual", 1);
        self.fanout.poke_channel(&found, cursor).await;
        self.fanout.wake();

        // Re-read: a revision may have landed between the first read and the close.
        if let Some(closed) = self.notifications.get(id).await? {
            return Ok(closed);
        }
        let mut closed = found;
        closed.close(now, Some(author));
        Ok(closed)
    }
}

/// `DELETE /api/v1/notifications/{id}` — delete and broadcast a tombstone.
#[derive(Clone)]
pub struct DeleteNotification {
    notifications: Arc<dyn NotificationRepository>,
    authorizer: Authorizer,
    clock: Arc<dyn Clock>,
    fanout: crate::fanout::Fanout,
}

impl core::fmt::Debug for DeleteNotification {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DeleteNotification").finish_non_exhaustive()
    }
}

impl DeleteNotification {
    #[must_use]
    pub fn new(
        notifications: Arc<dyn NotificationRepository>,
        authorizer: Authorizer,
        clock: Arc<dyn Clock>,
        fanout: crate::fanout::Fanout,
    ) -> Self {
        Self {
            notifications,
            authorizer,
            clock,
            fanout,
        }
    }

    /// Delete. Idempotent: an already-deleted notification succeeds and emits nothing.
    pub async fn execute(&self, caller: &Caller, id: NotificationId) -> Result<(), Failure> {
        let found = self
            .notifications
            .get(id)
            .await?
            .ok_or(DomainError::NotificationGone)?;

        // Deleting is a write, so it needs `publish` rather than `subscribe`.
        self.authorizer
            .authorize(caller, &found.channel, Scope::Publish)
            .await?;

        if let Some(cursor) = self
            .notifications
            .mark_deleted(id, self.clock.now())
            .await?
        {
            self.fanout.poke_channel(&found, cursor).await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use notify_domain::{
        Author, Channel, ChannelPolicy, ContentDraft, Membership, Principal, RevisionDraft, UserId,
    };
    use notify_ports::test_utils::{InMemory, TestClock, TestRateLimiter};
    use time::Duration;

    use super::*;
    use crate::fanout::Fanout;

    struct Fixture {
        store: InMemory,
        clock: TestClock,
        limiter: TestRateLimiter,
        inbox: ListInbox,
        get: GetNotification,
        revisions: ListRevisions,
        search: SearchNotifications,
        delete: DeleteNotification,
        unread: CountUnread,
        alice: Principal,
    }

    fn channel_id(id: &str) -> ChannelId {
        ChannelId::parse(id).unwrap()
    }

    fn fixture() -> Fixture {
        let store = InMemory::new();
        let clock = TestClock::new();
        let limiter = TestRateLimiter::new();
        let alice = Principal::User {
            id: UserId::generate(),
        };

        let mut channel = Channel::dynamic(channel_id("ci-infra"), None).unwrap();
        channel.upsert_member(
            Membership::new(alice.clone(), vec![Scope::Publish, Scope::Subscribe]).unwrap(),
        );
        store.seed_channel(channel);

        let authorizer = Authorizer::new(Arc::new(store.clone()));
        let get = GetNotification::new(
            Arc::new(store.clone()),
            authorizer.clone(),
            Arc::new(clock.clone()),
        );
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
            inbox: ListInbox::new(Arc::new(store.clone()), authorizer.clone()),
            revisions: ListRevisions::new(Arc::new(store.clone()), get.clone()),
            search: SearchNotifications::new(
                Arc::new(store.clone()),
                authorizer.clone(),
                Arc::new(limiter.clone()),
                Arc::new(clock.clone()),
            ),
            delete: DeleteNotification::new(
                Arc::new(store.clone()),
                authorizer.clone(),
                Arc::new(clock.clone()),
                fanout,
            ),
            unread: CountUnread::new(Arc::new(store.clone()), authorizer),
            get,
            store,
            clock,
            limiter,
            alice,
        }
    }

    async fn seed(fixture: &Fixture, channel: &str, title: &str) -> Notification {
        let (notification, written) = Notification::create(
            NotificationId::generate(),
            channel_id(channel),
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

    #[tokio::test]
    async fn the_inbox_defaults_to_every_channel_the_caller_reads() {
        let fixture = fixture();
        seed(&fixture, "ci-infra", "one").await;
        let caller = Caller::member(fixture.alice.clone());

        let page = fixture
            .inbox
            .execute(&caller, InboxFilters::default())
            .await
            .unwrap();
        assert_eq!(page.len(), 1);
    }

    #[tokio::test]
    async fn an_explicit_channel_filter_can_only_narrow() {
        let fixture = fixture();
        let bob = Principal::User {
            id: UserId::generate(),
        };
        let mut theirs = Channel::dynamic(channel_id("cluster-ovh-1"), None).unwrap();
        theirs.upsert_member(Membership::new(bob, vec![Scope::Subscribe]).unwrap());
        fixture.store.seed_channel(theirs);
        seed(&fixture, "cluster-ovh-1", "not for alice").await;

        let page = fixture
            .inbox
            .execute(
                &Caller::member(fixture.alice.clone()),
                InboxFilters {
                    channels: vec![channel_id("cluster-ovh-1")],
                    ..InboxFilters::default()
                },
            )
            .await
            .unwrap();

        assert!(
            page.is_empty(),
            "`?channels=` must not be a way into somebody else's channel"
        );
    }

    /// Mark a notification read straight at the port, keeping these tests about counting.
    async fn mark_seen(fixture: &Fixture, notification: NotificationId, who: &Principal) {
        notify_ports::ReceiptRepository::record(
            &fixture.store,
            &notify_domain::Receipt {
                id: notify_domain::ReceiptId::generate(),
                notification_id: notification,
                principal: who.clone(),
                device: None,
                kind: notify_domain::ReceiptKind::Seen,
                at: fixture.clock.now(),
            },
            // The default channel collects no receipts: the private read.
            notify_ports::Visibility::Reader,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn the_badge_counts_what_the_caller_has_not_read_and_stops_counting_what_they_have() {
        let fixture = fixture();
        let first = seed(&fixture, "ci-infra", "one").await;
        seed(&fixture, "ci-infra", "two").await;
        let caller = Caller::member(fixture.alice.clone());

        assert_eq!(
            fixture.unread.execute(&caller).await.unwrap(),
            vec![UnreadCount {
                channel: channel_id("ci-infra"),
                unread: 2,
            }]
        );

        mark_seen(&fixture, first.id, &fixture.alice).await;

        assert_eq!(
            fixture.unread.execute(&caller).await.unwrap(),
            vec![UnreadCount {
                channel: channel_id("ci-infra"),
                unread: 1,
            }],
            "reading one of two must decrement the badge, not clear it"
        );
    }

    #[tokio::test]
    async fn a_channel_with_nothing_unread_is_absent_rather_than_zero() {
        let fixture = fixture();
        let only = seed(&fixture, "ci-infra", "one").await;
        let caller = Caller::member(fixture.alice.clone());

        mark_seen(&fixture, only.id, &fixture.alice).await;

        // "No badge" has one representation: no entry.
        assert_eq!(
            fixture.unread.execute(&caller).await.unwrap(),
            [] as [notify_ports::UnreadCount; 0]
        );
    }

    #[tokio::test]
    async fn the_badge_never_counts_a_channel_the_caller_cannot_read() {
        let fixture = fixture();
        let bob = Principal::User {
            id: UserId::generate(),
        };
        let mut theirs = Channel::dynamic(channel_id("cluster-ovh-1"), None).unwrap();
        theirs.upsert_member(Membership::new(bob, vec![Scope::Subscribe]).unwrap());
        fixture.store.seed_channel(theirs);
        seed(&fixture, "cluster-ovh-1", "not for alice").await;

        // A channel alice is not in cannot reach the count.
        assert_eq!(
            fixture
                .unread
                .execute(&Caller::member(fixture.alice.clone()))
                .await
                .unwrap(),
            [] as [notify_ports::UnreadCount; 0]
        );
    }

    #[tokio::test]
    async fn one_reader_receipt_does_not_clear_another_readers_badge() {
        let fixture = fixture();
        let only = seed(&fixture, "ci-infra", "one").await;
        let bob = Principal::User {
            id: UserId::generate(),
        };
        let mut channel = Channel::dynamic(channel_id("ci-infra"), None).unwrap();
        channel
            .upsert_member(Membership::new(fixture.alice.clone(), vec![Scope::Subscribe]).unwrap());
        channel.upsert_member(Membership::new(bob.clone(), vec![Scope::Subscribe]).unwrap());
        fixture.store.seed_channel(channel);

        mark_seen(&fixture, only.id, &bob).await;

        assert_eq!(
            fixture
                .unread
                .execute(&Caller::member(fixture.alice.clone()))
                .await
                .unwrap(),
            vec![UnreadCount {
                channel: channel_id("ci-infra"),
                unread: 1,
            }],
            "unread is per principal; bob reading it says nothing about alice"
        );
    }

    #[tokio::test]
    async fn a_caller_in_no_channels_gets_an_empty_page_not_an_error() {
        let fixture = fixture();
        let stranger = Caller::member(Principal::User {
            id: UserId::generate(),
        });

        assert!(
            fixture
                .inbox
                .execute(&stranger, InboxFilters::default())
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn reading_one_notification_needs_its_channel() {
        let fixture = fixture();
        let notification = seed(&fixture, "ci-infra", "one").await;
        let stranger = Caller::member(Principal::User {
            id: UserId::generate(),
        });

        assert!(
            fixture
                .get
                .execute(&Caller::member(fixture.alice.clone()), notification.id)
                .await
                .is_ok()
        );
        assert_eq!(
            fixture
                .get
                .execute(&stranger, notification.id)
                .await
                .unwrap_err()
                .code(),
            Some("scope.missing")
        );
    }

    #[tokio::test]
    async fn an_expired_notification_reads_as_gone() {
        let fixture = fixture();
        // A simple notification ages from creation.
        let (notification, written) = Notification::create(
            NotificationId::generate(),
            channel_id("ci-infra"),
            None,
            RevisionDraft::simple(ContentDraft::titled("brief")),
            Author::anonymous(fixture.alice.clone()),
            fixture.clock.now(),
            &ChannelPolicy::default(),
        )
        .unwrap();
        NotificationRepository::insert(&fixture.store, &notification, &written.revision)
            .await
            .unwrap();

        fixture.clock.advance(Duration::days(8));
        assert_eq!(
            fixture
                .get
                .execute(&Caller::member(fixture.alice.clone()), notification.id)
                .await
                .unwrap_err()
                .code(),
            Some("notification.gone")
        );
    }

    #[tokio::test]
    async fn a_timeline_is_authorised_through_its_notification() {
        let fixture = fixture();
        let notification = seed(&fixture, "ci-infra", "one").await;
        let stranger = Caller::member(Principal::User {
            id: UserId::generate(),
        });

        assert!(
            fixture
                .revisions
                .execute(
                    &Caller::member(fixture.alice.clone()),
                    notification.id,
                    PageRequest::first()
                )
                .await
                .is_ok()
        );
        assert!(
            fixture
                .revisions
                .execute(&stranger, notification.id, PageRequest::first())
                .await
                .is_err()
        );
    }

    // --- T27 · Search stays scoped (design §3.4) --------------------------

    #[tokio::test]
    async fn critical_t27_search_is_scoped_to_the_callers_channels() {
        let fixture = fixture();
        let bob = Principal::User {
            id: UserId::generate(),
        };
        let mut theirs = Channel::dynamic(channel_id("cluster-ovh-1"), None).unwrap();
        theirs.upsert_member(Membership::new(bob.clone(), vec![Scope::Subscribe]).unwrap());
        fixture.store.seed_channel(theirs);

        seed(&fixture, "ci-infra", "etcd degraded").await;
        seed(&fixture, "cluster-ovh-1", "etcd degraded").await;

        let hers = fixture
            .search
            .execute(
                &Caller::member(fixture.alice.clone()),
                "etcd",
                PageRequest::first(),
            )
            .await
            .unwrap();
        assert_eq!(hers.len(), 1);
        assert_eq!(hers.items[0].channel, channel_id("ci-infra"));
    }

    #[tokio::test]
    async fn an_empty_query_searches_nothing() {
        let fixture = fixture();
        seed(&fixture, "ci-infra", "etcd degraded").await;

        assert!(
            fixture
                .search
                .execute(
                    &Caller::member(fixture.alice.clone()),
                    "   ",
                    PageRequest::first()
                )
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn search_is_rate_limited_per_principal() {
        let fixture = fixture();
        fixture.limiter.limit(Quota::Search {
            principal: fixture.alice.reference(),
        });

        assert!(matches!(
            fixture
                .search
                .execute(
                    &Caller::member(fixture.alice.clone()),
                    "etcd",
                    PageRequest::first()
                )
                .await
                .unwrap_err(),
            Failure::RateLimited { .. }
        ));
    }

    #[tokio::test]
    async fn deleting_broadcasts_one_tombstone_and_is_idempotent() {
        let fixture = fixture();
        let notification = seed(&fixture, "ci-infra", "one").await;
        let caller = Caller::member(fixture.alice.clone());

        fixture
            .delete
            .execute(&caller, notification.id)
            .await
            .unwrap();
        fixture
            .delete
            .execute(&caller, notification.id)
            .await
            .unwrap();

        assert_eq!(
            fixture.store.pokes().len(),
            1,
            "a retried delete must not emit a second tombstone"
        );
    }

    #[tokio::test]
    async fn deleting_needs_a_write_scope() {
        let fixture = fixture();
        let notification = seed(&fixture, "ci-infra", "one").await;

        let reader = Principal::User {
            id: UserId::generate(),
        };
        let mut channel = Channel::dynamic(channel_id("ci-infra"), None).unwrap();
        channel.upsert_member(Membership::new(reader.clone(), vec![Scope::Subscribe]).unwrap());
        channel.upsert_member(
            Membership::new(
                fixture.alice.clone(),
                vec![Scope::Publish, Scope::Subscribe],
            )
            .unwrap(),
        );
        notify_ports::ChannelRepository::upsert(&fixture.store, &channel)
            .await
            .unwrap();

        assert!(
            fixture
                .delete
                .execute(&Caller::member(reader), notification.id)
                .await
                .is_err(),
            "reading a channel does not entitle you to delete from it"
        );
    }

    fn closer(fixture: &Fixture) -> CloseNotification {
        let fanout = Fanout::new(
            Arc::new(fixture.store.clone()),
            Arc::new(fixture.store.clone()),
            Arc::new(fixture.store.clone()),
            Arc::new(fixture.store.clone()),
            Arc::new(fixture.store.clone()),
            Arc::new(fixture.clock.clone()),
            4_096,
        );
        CloseNotification::new(
            Arc::new(fixture.store.clone()),
            Authorizer::new(Arc::new(fixture.store.clone())),
            Arc::new(fixture.clock.clone()),
            fanout,
            crate::attribution::Attribution::new(
                Arc::new(fixture.store.clone()),
                Arc::new(fixture.store.clone()),
            ),
            Arc::new(notify_ports::NoMetrics),
        )
    }

    /// `ci-infra` with alice (publish), a manager, a reader; and `elsewhere` without alice.
    async fn members(fixture: &Fixture) -> (Principal, Principal) {
        let manager = Principal::User {
            id: UserId::generate(),
        };
        let reader = Principal::User {
            id: UserId::generate(),
        };
        let mut channel = Channel::dynamic(channel_id("ci-infra"), None).unwrap();
        for (who, scopes) in [
            (
                fixture.alice.clone(),
                vec![Scope::Publish, Scope::Subscribe],
            ),
            (manager.clone(), vec![Scope::Manage]),
            (reader.clone(), vec![Scope::Subscribe]),
        ] {
            channel.upsert_member(Membership::new(who, scopes).unwrap());
        }
        notify_ports::ChannelRepository::upsert(&fixture.store, &channel)
            .await
            .unwrap();
        let mut other = Channel::dynamic(channel_id("elsewhere"), None).unwrap();
        other.upsert_member(Membership::new(reader.clone(), vec![Scope::Publish]).unwrap());
        fixture.store.seed_channel(other);
        (manager, reader)
    }

    #[tokio::test]
    async fn publish_or_manage_closes_once_and_names_the_closer() {
        let fixture = fixture();
        let (manager, _) = members(&fixture).await;
        let close = closer(&fixture);
        let first = seed(&fixture, "ci-infra", "CVE").await;
        let second = seed(&fixture, "ci-infra", "CVE 2").await;

        let closed = close
            .execute(&Caller::member(fixture.alice.clone()), first.id)
            .await
            .unwrap();
        assert_eq!(closed.state(), State::Closed);
        assert_eq!(
            closed.closed_by.as_ref().map(|by| by.principal.clone()),
            Some(fixture.alice.clone())
        );
        let by_manager = close
            .execute(&Caller::member(manager.clone()), second.id)
            .await
            .unwrap();
        assert_eq!(
            by_manager.closed_by.map(|by| by.principal),
            Some(manager.clone())
        );

        // Again, by somebody else: nothing written, the first closer kept.
        let pokes = fixture.store.pokes().len();
        let again = close
            .execute(&Caller::member(manager), first.id)
            .await
            .unwrap();
        assert_eq!(fixture.store.pokes().len(), pokes);
        assert_eq!(
            again.closed_by.map(|by| by.principal),
            Some(fixture.alice.clone())
        );
        let stored = fixture.store.get(first.id).await.unwrap().unwrap();
        assert!(stored.closed_by.is_some());
    }

    #[tokio::test]
    async fn a_reader_a_stranger_and_an_unknown_id_are_one_refusal() {
        let fixture = fixture();
        let (_, reader) = members(&fixture).await;
        let close = closer(&fixture);
        let notification = seed(&fixture, "ci-infra", "CVE").await;

        let as_reader = close
            .execute(&Caller::member(reader.clone()), notification.id)
            .await
            .unwrap_err();
        let unknown = close
            .execute(&Caller::member(reader), NotificationId::generate())
            .await
            .unwrap_err();
        assert_eq!(as_reader.to_string(), unknown.to_string());
        assert_eq!(as_reader.code(), Some("scope.missing"));
        let stranger = Principal::User {
            id: UserId::generate(),
        };
        assert!(
            close
                .execute(&Caller::member(stranger), notification.id)
                .await
                .is_err()
        );
        let stored = fixture.store.get(notification.id).await.unwrap().unwrap();
        assert_eq!(stored.state(), State::Open);
    }

    #[tokio::test]
    async fn a_deleted_notification_is_gone() {
        let fixture = fixture();
        members(&fixture).await;
        let notification = seed(&fixture, "ci-infra", "CVE").await;
        let caller = Caller::member(fixture.alice.clone());
        fixture
            .delete
            .execute(&caller, notification.id)
            .await
            .unwrap();

        let refused = closer(&fixture)
            .execute(&caller, notification.id)
            .await
            .unwrap_err();
        assert_eq!(refused.code(), Some("notification.gone"));
    }

    #[test]
    fn scopes_in_lists_the_callers_own() {
        let alice = Principal::User {
            id: UserId::generate(),
        };
        let bob = Principal::User {
            id: UserId::generate(),
        };
        let mut channel = Channel::dynamic(channel_id("ci-infra"), None).unwrap();
        channel.upsert_member(
            Membership::new(alice.clone(), vec![Scope::Publish, Scope::Subscribe]).unwrap(),
        );
        channel.upsert_member(Membership::new(bob.clone(), vec![Scope::Manage]).unwrap());

        assert_eq!(
            Authorizer::scopes_in(&Caller::member(alice), &channel),
            [Scope::Subscribe, Scope::Publish]
        );
        assert_eq!(
            Authorizer::scopes_in(&Caller::member(bob), &channel),
            [Scope::Manage]
        );
    }
}
