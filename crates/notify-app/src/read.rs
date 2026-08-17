//! Reading notifications: the inbox, one notification, its timeline, and search.
//!
//! Every read is scoped to the channels the caller may subscribe to, resolved once
//! through [`Authorizer::readable_channels`]. An empty scope returns an empty page
//! rather than an error — "you are in no channels" is a valid state, not a failure.

use std::sync::Arc;

use notify_domain::{
    ChannelId, DomainError, Notification, NotificationId, Priority, Revision, Scope, State, Status,
};
use notify_ports::{
    Allowance, Clock, Failure, InboxQuery, NotificationRepository, Page, PageRequest, Quota,
    RateLimiter, SearchQuery, UnreadCount,
};

use crate::authz::{Authorizer, Caller};

/// `GET /api/v1/unread` — the badge (design §7.3, §9.2, §10.4, §10bis).
///
/// # Why this is a route of its own
///
/// It could have hung off `GET /channels`, which is what the sidebar renders. It
/// does not, and the reason is how the two change: a channel list changes when an
/// operator edits configuration, and is cached accordingly; an unread count
/// changes every time anybody publishes or anybody reads. Folding the second into
/// the first would mean either re-fetching the channel list constantly or showing
/// a badge as stale as the configuration.
///
/// Flat under `/api/v1`, next to `/search`, `/sync` and `/receipts`, rather than
/// under an `/inbox` prefix that would have exactly one member: like those, this
/// is a question about everything the caller can read, not about one resource.
///
/// # What "unread" means here, stated once
///
/// A notification the caller may read, still live, with no `seen` receipt from
/// them. `seen` is recorded on every channel since the increment that added this
/// (§7.3): the channel's `receipts_enabled` policy governs whether *other people*
/// are shown who read it, never whether the reading is recorded. Without that,
/// this count would be permanently equal to the channel's size wherever an
/// operator had left receipts off — a badge that only ever grows.
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
    /// Wire it.
    #[must_use]
    pub fn new(notifications: Arc<dyn NotificationRepository>, authorizer: Authorizer) -> Self {
        Self {
            notifications,
            authorizer,
        }
    }

    /// Count, over every channel the caller may read.
    ///
    /// No channel filter parameter, unlike the inbox: a badge is drawn for the
    /// whole sidebar at once, and a caller wanting one channel reads the one entry
    /// out of the answer rather than paying a round trip per channel.
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
    /// Restrict to these channels. Empty means every channel the caller may read —
    /// which is what the unified inbox asks for.
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
    /// Wire it.
    #[must_use]
    pub fn new(notifications: Arc<dyn NotificationRepository>, authorizer: Authorizer) -> Self {
        Self {
            notifications,
            authorizer,
        }
    }

    /// List.
    pub async fn execute(
        &self,
        caller: &Caller,
        filters: InboxFilters,
    ) -> Result<Page<Notification>, Failure> {
        let readable = self.authorizer.readable_channels(caller).await?;

        // An explicit channel list narrows what the caller may already see; it can
        // never widen it. Doing the intersection here rather than trusting the
        // caller is what keeps `?channels=` from being an access-control bypass.
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
    /// Wire it.
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

    /// Fetch, checking the caller may read its channel.
    ///
    /// A notification that is gone answers `notification.gone` rather than a bare
    /// 404, but only *after* the channel check: otherwise "gone" versus "no such
    /// notification" would tell a stranger which ids exist.
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
    /// Wire it.
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
        // Authorised through the notification, so the timeline cannot be read by
        // anyone who could not read the notification itself.
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
    /// Wire it.
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
        // Full text is the expensive read; the palette debounces client-side, this
        // is the floor under that (design §8.2).
        let quota = Quota::Search {
            principal: caller.principal.reference(),
        };
        if let Allowance::Limited {
            retry_after_seconds,
            // No override: §8.2 makes `limits` a property of a channel, and a search
            // crosses every channel the caller can read.
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
    /// Wire it.
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

    /// Delete. Idempotent: deleting an already-deleted notification succeeds and
    /// emits nothing, so a retried request cannot produce a second tombstone.
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
    #![allow(clippy::unwrap_used, reason = "a failing unwrap is a failing test")]

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

    /// Mark a notification read, the way `PostReceipt` eventually does.
    ///
    /// Straight at the port rather than through the use case: what is under test
    /// here is the counting, and routing it through the receipt path would make
    /// these tests fail for reasons that belong to `interact.rs`.
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
            // The channel here collects no receipts, which is the default and the
            // case the badge has to work in — so this is the private read.
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

        // Not `[{channel, unread: 0}]`. "No badge" has one representation, so that
        // a client renders a badge per entry instead of filtering the answer first.
        assert!(fixture.unread.execute(&caller).await.unwrap().is_empty());
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

        // The count is taken over `readable_channels`, so a channel alice is not in
        // cannot reach the answer — a badge is a smaller leak than a listing, and a
        // leak all the same.
        assert!(
            fixture
                .unread
                .execute(&Caller::member(fixture.alice.clone()))
                .await
                .unwrap()
                .is_empty()
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
}
