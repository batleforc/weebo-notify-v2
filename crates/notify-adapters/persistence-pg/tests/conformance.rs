//! Every query in this adapter, run against a real Postgres.
//!
//! This suite is what makes the decision not to use `sqlx::query!` defensible (see
//! the crate docs): the macros would check the SQL at compile time, these tests
//! check it against a live server *and* check that the semantics match the
//! in-memory double the domain and use cases were written against. A new method on
//! this adapter comes with a case here, or the trade stops paying for itself.
//!
//! Everything lives under `mod e2e` so `task test` skips it and `task test:e2e`
//! runs it. Without `WNK_TEST_DATABASE_URL` (or `DATABASE_URL`) each case reports
//! that it was skipped rather than failing — a laptop with no database still gets a
//! green `task test`, and the `e2e` job always has one.
//!
//! Tests do not truncate anything and share the database happily: every one of them
//! works in a channel of its own, named after a fresh ULID, so they can run in
//! parallel and leave evidence behind for whoever is debugging.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a failing assertion is a failing test"
)]

mod e2e {
    use std::sync::Arc;

    use notify_domain::{
        ApiKey, ApiKeyId, ApiKeyOwner, ApiKeyPrefix, AttachmentId, Author, Channel, ChannelId,
        ChannelPolicy, ContentDraft, Cursor, Day, Device, DeviceId, DevicePublicKey, Dnd, DndScope,
        ExternalId, GroupBinding, HumanDuration, Image, Interaction, InteractionLog, Membership,
        Notification, NotificationId, Principal, Priority, PushTransport, QuietHours, Receipt,
        ReceiptKind, RevisionDraft, Scope, ServiceAccount, ServiceAccountId, Silence,
        SilenceTarget, State, Status, TombstoneReason, Ttl, User, UserId, WallClock,
    };
    use notify_persistence_pg::{PgLeaderElection, PgStore};
    use notify_ports::{
        Allowance, ApiKeyRepository, AttachmentRegistry, AuditEntry, AuditLog, AuditQuery,
        ChannelRepository, Claim, Declaration, DeclarationKind, DeclarationStore, DeviceRepository,
        EventPayload, EventStore, IdempotencyStore, InboxQuery, InteractionRepository,
        LeaderElection, NotificationRepository, PageRequest, PreferenceRepository, Quota,
        RateLimiter, ReceiptRepository, ReplayGuard, SearchQuery, ServiceAccountRepository,
        StoredApiKey, SyncQuery, UnreadCount, UserRepository, Visibility,
    };
    use time::{Duration, OffsetDateTime};

    /// The database under test, or `None` when there is not one.
    async fn store() -> Option<PgStore> {
        let url = std::env::var("WNK_TEST_DATABASE_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .ok()?;

        let store = PgStore::connect(&url, 8)
            .await
            .expect("the configured database accepts connections");
        store.migrate().await.expect("migrations apply");

        Some(store)
    }

    /// Skip cleanly when no database is configured.
    macro_rules! store {
        () => {
            match store().await {
                Some(store) => store,
                None => {
                    eprintln!("no WNK_TEST_DATABASE_URL or DATABASE_URL — skipping");
                    return;
                }
            }
        };
    }

    // One `PgStore` implements eleven ports, so `store.list(…)` is genuinely
    // ambiguous. These name the port instead of scattering `Trait::method(&store,
    // …)` over every call, and they read like what the use cases actually hold.
    fn notifications(store: &PgStore) -> &dyn NotificationRepository {
        store
    }
    fn registry(store: &PgStore) -> &dyn AttachmentRegistry {
        store
    }
    fn channels(store: &PgStore) -> &dyn ChannelRepository {
        store
    }
    fn users(store: &PgStore) -> &dyn UserRepository {
        store
    }
    fn accounts(store: &PgStore) -> &dyn ServiceAccountRepository {
        store
    }
    fn keys(store: &PgStore) -> &dyn ApiKeyRepository {
        store
    }
    fn devices(store: &PgStore) -> &dyn DeviceRepository {
        store
    }
    fn events(store: &PgStore) -> &dyn EventStore {
        store
    }
    fn audit(store: &PgStore) -> &dyn AuditLog {
        store
    }
    fn declarations(store: &PgStore) -> &dyn DeclarationStore {
        store
    }
    fn receipts(store: &PgStore) -> &dyn ReceiptRepository {
        store
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }

    /// A channel name nothing else in the suite will use.
    fn fresh_channel() -> ChannelId {
        ChannelId::parse(&format!("t-{}", ulid_slug())).unwrap()
    }

    /// A group name nothing else in the suite will use.
    ///
    /// Group bindings are keyed by name and this suite leaves its rows behind, so a
    /// shared `infra-admins` would make every run see the previous one's channels.
    fn fresh_group() -> String {
        format!("g-{}", ulid_slug())
    }

    /// Eight characters no other key in the suite will take.
    ///
    /// The *tail* of a ULID: the leading characters are its millisecond timestamp,
    /// so three keys minted in the same test would all share them and collide on
    /// the unique index — which is a fixture bug that reads exactly like a real one.
    fn fresh_prefix() -> String {
        let slug = ulid_slug();
        slug[slug.len() - 8..].to_owned()
    }

    fn ulid_slug() -> String {
        NotificationId::generate()
            .to_string()
            .trim_start_matches("ntf_")
            .to_lowercase()
    }

    fn user() -> (UserId, Principal) {
        let id = UserId::generate();
        (id, Principal::User { id })
    }

    fn author(principal: &Principal) -> Author {
        Author::anonymous(principal.clone())
    }

    /// A channel with one member holding every scope, already stored.
    async fn seeded_channel(store: &PgStore, member: &Principal) -> ChannelId {
        let id = fresh_channel();
        let mut channel = Channel::dynamic(id.clone(), None).unwrap();
        channel.upsert_member(
            Membership::new(
                member.clone(),
                vec![Scope::Publish, Scope::Subscribe, Scope::Manage],
            )
            .unwrap(),
        );
        channels(store).upsert(&channel).await.unwrap();

        id
    }

    /// Publish through the repository and hand back what was stored.
    async fn publish(
        store: &PgStore,
        channel: &ChannelId,
        principal: &Principal,
        title: &str,
    ) -> (Notification, Cursor) {
        publish_with(
            store,
            channel,
            principal,
            RevisionDraft::lifecycle(State::Open, ContentDraft::titled(title)),
            None,
            now(),
        )
        .await
    }

    async fn publish_with(
        store: &PgStore,
        channel: &ChannelId,
        principal: &Principal,
        draft: RevisionDraft,
        external_id: Option<ExternalId>,
        at: OffsetDateTime,
    ) -> (Notification, Cursor) {
        let (notification, written) = Notification::create(
            NotificationId::generate(),
            channel.clone(),
            external_id,
            draft,
            author(principal),
            at,
            &ChannelPolicy::default(),
        )
        .unwrap();

        let cursor = notifications(store)
            .insert(&notification, &written.revision)
            .await
            .unwrap();

        (notification, cursor)
    }

    fn inbox(channels: &[ChannelId]) -> InboxQuery {
        InboxQuery {
            channels: channels.to_vec(),
            ..InboxQuery::default()
        }
    }

    // --- Notifications -----------------------------------------------------

    #[tokio::test]
    async fn a_published_notification_comes_back_with_its_head_revision() {
        let store = store!();
        let (_, alice) = user();
        let channel = seeded_channel(&store, &alice).await;

        let (published, cursor) = publish(&store, &channel, &alice, "deploy prod").await;

        let found = NotificationRepository::get(&store, published.id)
            .await
            .unwrap()
            .expect("it was just written");

        assert_eq!(found.id, published.id);
        assert_eq!(found.channel, channel);
        assert_eq!(found.head.seq, 1);
        assert_eq!(found.head.content.title, "deploy prod");
        assert_eq!(found.head.state, State::Open);
        assert_eq!(found.last_seq, 1);
        assert!(cursor > Cursor::START, "a write reports where it landed");
    }

    #[tokio::test]
    async fn a_revision_moves_the_head_and_keeps_the_history() {
        let store = store!();
        let (_, alice) = user();
        let channel = seeded_channel(&store, &alice).await;
        let (mut published, first) = publish(&store, &channel, &alice, "deploy prod").await;

        let written = published
            .revise(
                RevisionDraft {
                    state: Some(State::Closed),
                    status: Some(Status::Success),
                    ..RevisionDraft::simple(ContentDraft::default())
                },
                author(&alice),
                now(),
                &ChannelPolicy::default(),
            )
            .unwrap();
        let second = store
            .append_revision(&published, &written.revision)
            .await
            .unwrap();

        assert!(second > first, "the cursor only ever moves forwards");

        let found = NotificationRepository::get(&store, published.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.head.seq, 2);
        assert_eq!(found.head.state, State::Closed);
        assert_eq!(found.head.status, Status::Success);
        assert_eq!(
            found.head.content.title, "deploy prod",
            "absent content keeps what the head says (design §2.2)"
        );

        let history = store
            .revisions(published.id, PageRequest::first())
            .await
            .unwrap();
        assert_eq!(history.items.len(), 2);
        assert_eq!(history.items[0].seq, 1, "a timeline is read forwards");
        assert_eq!(history.items[1].seq, 2);
    }

    #[tokio::test]
    async fn two_publishers_racing_on_one_external_id_produce_one_notification() {
        let store = store!();
        let (_, alice) = user();
        let channel = seeded_channel(&store, &alice).await;
        let external = ExternalId::parse("run-4521").unwrap();

        publish_with(
            &store,
            &channel,
            &alice,
            RevisionDraft::lifecycle(State::Open, ContentDraft::titled("first")),
            Some(external.clone()),
            now(),
        )
        .await;

        // The second insert loses the unique index, and losing it has to be
        // recoverable: the use case re-reads and revises instead (scenario T16).
        let (loser, written) = Notification::create(
            NotificationId::generate(),
            channel.clone(),
            Some(external.clone()),
            RevisionDraft::lifecycle(State::Open, ContentDraft::titled("second")),
            author(&alice),
            now(),
            &ChannelPolicy::default(),
        )
        .unwrap();

        let refusal = notifications(&store)
            .insert(&loser, &written.revision)
            .await
            .expect_err("the index refuses the duplicate");

        assert!(
            refusal.is_retryable(),
            "an upsert that loses this race re-reads and carries on"
        );

        let found = store
            .find_by_external_id(&channel, &external)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.head.content.title, "first");
    }

    #[tokio::test]
    async fn the_inbox_narrows_on_every_filter_it_offers() {
        let store = store!();
        let (_, alice) = user();
        let channel = seeded_channel(&store, &alice).await;

        publish_with(
            &store,
            &channel,
            &alice,
            RevisionDraft {
                priority: Some(Priority::Urgent),
                ..RevisionDraft::lifecycle(
                    State::Working,
                    ContentDraft {
                        tags: vec!["prod".to_owned(), "deploy".to_owned()],
                        group_key: Some("release-9".to_owned()),
                        ..ContentDraft::titled("urgent one")
                    },
                )
            },
            None,
            now(),
        )
        .await;
        publish(&store, &channel, &alice, "ordinary one").await;

        let all = notifications(&store)
            .list(&inbox(std::slice::from_ref(&channel)))
            .await
            .unwrap();
        assert_eq!(all.items.len(), 2);
        assert_eq!(
            all.items[0].head.content.title, "ordinary one",
            "most recent activity first"
        );

        let by_state = notifications(&store)
            .list(&InboxQuery {
                states: vec![State::Working],
                ..inbox(std::slice::from_ref(&channel))
            })
            .await
            .unwrap();
        assert_eq!(by_state.items.len(), 1);

        let by_priority = notifications(&store)
            .list(&InboxQuery {
                min_priority: Some(Priority::High),
                ..inbox(std::slice::from_ref(&channel))
            })
            .await
            .unwrap();
        assert_eq!(by_priority.items.len(), 1);

        let by_tag = notifications(&store)
            .list(&InboxQuery {
                tags: vec!["prod".to_owned()],
                ..inbox(std::slice::from_ref(&channel))
            })
            .await
            .unwrap();
        assert_eq!(by_tag.items.len(), 1);

        let by_missing_tag = notifications(&store)
            .list(&InboxQuery {
                tags: vec!["prod".to_owned(), "absent".to_owned()],
                ..inbox(std::slice::from_ref(&channel))
            })
            .await
            .unwrap();
        assert!(by_missing_tag.is_empty(), "tags narrow with AND, not OR");

        let by_group = notifications(&store)
            .list(&InboxQuery {
                group_key: Some("release-9".to_owned()),
                ..inbox(std::slice::from_ref(&channel))
            })
            .await
            .unwrap();
        assert_eq!(by_group.items.len(), 1);

        let by_status = notifications(&store)
            .list(&InboxQuery {
                statuses: vec![Status::Failure],
                ..inbox(std::slice::from_ref(&channel))
            })
            .await
            .unwrap();
        assert!(by_status.is_empty());
    }

    #[tokio::test]
    async fn an_inbox_page_follows_its_cursor_without_gaps_or_repeats() {
        let store = store!();
        let (_, alice) = user();
        let channel = seeded_channel(&store, &alice).await;

        for index in 0..5 {
            publish(&store, &channel, &alice, &format!("entry {index}")).await;
        }

        let first = notifications(&store)
            .list(&InboxQuery {
                page: PageRequest::of_size(2),
                ..inbox(std::slice::from_ref(&channel))
            })
            .await
            .unwrap();
        assert_eq!(first.items.len(), 2);
        let next = first.next.expect("there is more behind it");

        let second = notifications(&store)
            .list(&InboxQuery {
                page: PageRequest::of_size(2).after(next),
                ..inbox(std::slice::from_ref(&channel))
            })
            .await
            .unwrap();
        assert_eq!(second.items.len(), 2);

        let seen: Vec<_> = first
            .items
            .iter()
            .chain(&second.items)
            .map(|entry| entry.id)
            .collect();
        let mut unique = seen.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            seen.len(),
            unique.len(),
            "offsets repeat rows; cursors do not"
        );
    }

    #[tokio::test]
    async fn a_search_cannot_reach_outside_the_channels_it_was_given() {
        let store = store!();
        let (_, alice) = user();
        let (_, bob) = user();
        let mine = seeded_channel(&store, &alice).await;
        let theirs = seeded_channel(&store, &bob).await;

        publish(&store, &mine, &alice, "etcd degraded on cluster").await;
        publish(&store, &theirs, &bob, "etcd degraded elsewhere").await;

        let found = store
            .search(&SearchQuery {
                text: "etcd".to_owned(),
                channels: vec![mine.clone()],
                page: PageRequest::first(),
            })
            .await
            .unwrap();

        assert_eq!(found.items.len(), 1, "scope first, match second (T27)");
        assert_eq!(found.items[0].channel, mine);

        let nothing = store
            .search(&SearchQuery {
                text: "etcd".to_owned(),
                channels: Vec::new(),
                page: PageRequest::first(),
            })
            .await
            .unwrap();
        assert!(
            nothing.is_empty(),
            "no channels means nothing, not everything"
        );
    }

    #[tokio::test]
    async fn deleting_twice_emits_one_tombstone() {
        let store = store!();
        let (_, alice) = user();
        let channel = seeded_channel(&store, &alice).await;
        let (published, _) = publish(&store, &channel, &alice, "mistake").await;

        let first = store.mark_deleted(published.id, now()).await.unwrap();
        let second = store.mark_deleted(published.id, now()).await.unwrap();

        assert!(first.is_some());
        assert!(second.is_none(), "idempotent, and no second broadcast");

        let still_there = NotificationRepository::get(&store, published.id)
            .await
            .unwrap()
            .expect("get is the one read that sees a tombstone");
        assert!(still_there.deleted_at.is_some());

        let listed = notifications(&store)
            .list(&inbox(std::slice::from_ref(&channel)))
            .await
            .unwrap();
        assert!(listed.is_empty(), "reads never resurrect what is gone");
    }

    #[tokio::test]
    async fn critical_t9_an_undelivered_expired_notification_is_counted_not_backlogged() {
        let store = store!();
        let (alice_id, alice) = user();
        let channel = seeded_channel(&store, &alice).await;

        // Closed immediately with a short TTL, so retention starts and runs out.
        let started = now() - Duration::days(30);
        let (mut published, _) = publish_with(
            &store,
            &channel,
            &alice,
            RevisionDraft {
                state: Some(State::Closed),
                ttl: Some(Ttl::For(HumanDuration::parse("1h", "ttl").unwrap())),
                ..RevisionDraft::simple(ContentDraft::titled("nobody saw this"))
            },
            None,
            started,
        )
        .await;
        published.deleted_at = None;

        let claimed = store.claim_expired(now(), 100).await.unwrap();
        let mine: Vec<_> = claimed
            .iter()
            .filter(|entry| entry.channel == channel)
            .collect();

        assert_eq!(mine.len(), 1);
        assert!(!mine[0].was_delivered);

        let page = store
            .sync(
                alice_id,
                &SyncQuery {
                    after: None,
                    channels: vec![channel.clone()],
                    groups: Vec::new(),
                    page: PageRequest::first(),
                },
            )
            .await
            .unwrap();

        assert!(
            !page
                .events
                .iter()
                .any(|event| matches!(&event.payload, EventPayload::Tombstone { .. })),
            "an expired notification nobody saw does not turn up a week late"
        );
        assert_eq!(page.expired.len(), 1);
        assert_eq!(page.expired[0].channel, channel);
        assert_eq!(page.expired[0].count, 1);
    }

    #[tokio::test]
    async fn an_expired_notification_somebody_saw_gets_a_tombstone() {
        let store = store!();
        let (alice_id, alice) = user();
        let channel = seeded_channel(&store, &alice).await;

        let (published, _) = publish_with(
            &store,
            &channel,
            &alice,
            RevisionDraft {
                state: Some(State::Closed),
                ttl: Some(Ttl::For(HumanDuration::parse("1h", "ttl").unwrap())),
                ..RevisionDraft::simple(ContentDraft::titled("seen then expired"))
            },
            None,
            now() - Duration::days(30),
        )
        .await;

        receipts(&store)
            .record(
                &Receipt {
                    id: notify_domain::ReceiptId::generate(),
                    notification_id: published.id,
                    principal: alice.clone(),
                    device: None,
                    kind: ReceiptKind::Seen,
                    at: now() - Duration::days(29),
                },
                Visibility::Channel,
            )
            .await
            .unwrap();

        store.claim_expired(now(), 100).await.unwrap();

        let page = store
            .sync(
                alice_id,
                &SyncQuery {
                    after: None,
                    channels: vec![channel],
                    groups: Vec::new(),
                    page: PageRequest::of_size(200),
                },
            )
            .await
            .unwrap();

        assert!(
            page.events.iter().any(|event| matches!(
                &event.payload,
                EventPayload::Tombstone { reason, .. } if *reason == TombstoneReason::Expired
            )),
            "somebody has it on screen and needs to drop it"
        );
        assert!(page.expired.is_empty());
    }

    #[tokio::test]
    async fn the_attachments_of_doomed_rows_are_named_and_the_others_are_not() {
        // The JSON path this walks — `content->'image'->>'ref'` — is the kind of
        // thing that type-checks in Rust and matches nothing in Postgres, so it is
        // asserted against a real database rather than against the in-memory double
        // that shares none of its semantics.
        let store = store!();
        let (_, alice) = user();
        let channel = seeded_channel(&store, &alice).await;

        let doomed_image = AttachmentId::generate();
        let shared_image = AttachmentId::generate();

        let with_image = |reference| {
            RevisionDraft::lifecycle(
                State::Open,
                ContentDraft {
                    image: Some(Image::Attachment { reference }),
                    ..ContentDraft::titled("screenshot")
                },
            )
        };

        let (doomed, _) = publish_with(
            &store,
            &channel,
            &alice,
            with_image(doomed_image),
            None,
            now(),
        )
        .await;
        let (also_doomed, _) = publish_with(
            &store,
            &channel,
            &alice,
            with_image(shared_image),
            None,
            now(),
        )
        .await;
        // Names the same object and is *not* on its way out.
        let (_, _) = publish_with(
            &store,
            &channel,
            &alice,
            with_image(shared_image),
            None,
            now(),
        )
        .await;
        // No image at all: must not appear as a stray `NULL`.
        let (plain, _) = publish(&store, &channel, &alice, "no picture").await;

        for id in [doomed.id, also_doomed.id, plain.id] {
            store
                .mark_deleted(id, now() - Duration::days(10))
                .await
                .unwrap();
        }

        let orphaned = store
            .orphaned_attachments(now() - Duration::days(7), 100)
            .await
            .unwrap();

        assert!(
            orphaned.contains(&doomed_image),
            "an object nothing else references goes with its notification"
        );
        assert!(
            !orphaned.contains(&shared_image),
            "a live notification still needs it — deleting it would be data loss"
        );
    }

    #[tokio::test]
    async fn a_signed_upload_nobody_referenced_becomes_collectable_once_it_is_old() {
        // D25, against a real schema: the registry is the only record of an id until
        // a revision names it, and the `NOT EXISTS` join is what decides whether one
        // ever did.
        let store = store!();
        let (_, alice) = user();
        let channel = seeded_channel(&store, &alice).await;

        let orphan = AttachmentId::generate();
        let recent = AttachmentId::generate();
        registry(&store)
            .record(orphan, &channel, now() - Duration::days(3))
            .await
            .unwrap();
        registry(&store)
            .record(recent, &channel, now())
            .await
            .unwrap();

        let stale = registry(&store)
            .unreferenced(now() - Duration::days(1), 100)
            .await
            .unwrap();

        assert!(stale.contains(&orphan), "signed three days ago, never used");
        assert!(
            !stale.contains(&recent),
            "signed just now — the publisher may still be uploading"
        );

        // And it stops being collectable the moment something references it.
        let referenced = AttachmentId::generate();
        registry(&store)
            .record(referenced, &channel, now() - Duration::days(3))
            .await
            .unwrap();
        publish_with(
            &store,
            &channel,
            &alice,
            RevisionDraft::lifecycle(
                State::Open,
                ContentDraft {
                    image: Some(Image::Attachment {
                        reference: referenced,
                    }),
                    ..ContentDraft::titled("used it")
                },
            ),
            None,
            now(),
        )
        .await;

        let stale = registry(&store)
            .unreferenced(now() - Duration::days(1), 100)
            .await
            .unwrap();
        assert!(!stale.contains(&referenced));

        // Forgetting is what keeps the table from growing with every upload ever.
        assert_eq!(registry(&store).forget(&[orphan]).await.unwrap(), 1);
        let stale = registry(&store)
            .unreferenced(now() - Duration::days(1), 100)
            .await
            .unwrap();
        assert!(!stale.contains(&orphan));
    }

    #[tokio::test]
    async fn an_attachment_is_visible_exactly_where_a_notification_shows_it() {
        // The authorisation behind `GET /attachments/{id}`, against a real schema:
        // an attachment has no access control of its own, so what entitles somebody
        // to the bytes is being able to read something that shows them.
        let store = store!();
        let (_, alice) = user();
        let channel = seeded_channel(&store, &alice).await;
        let elsewhere = seeded_channel(&store, &alice).await;

        let reference = AttachmentId::generate();
        let (shown, _) = publish_with(
            &store,
            &channel,
            &alice,
            RevisionDraft::lifecycle(
                State::Open,
                ContentDraft {
                    image: Some(Image::Attachment { reference }),
                    ..ContentDraft::titled("screenshot")
                },
            ),
            None,
            now(),
        )
        .await;

        assert!(
            notifications(&store)
                .attachment_is_visible(&reference, std::slice::from_ref(&channel))
                .await
                .unwrap()
        );
        assert!(
            !notifications(&store)
                .attachment_is_visible(&reference, &[elsewhere])
                .await
                .unwrap(),
            "a channel that does not show it grants nothing"
        );
        assert!(
            !notifications(&store)
                .attachment_is_visible(&reference, &[])
                .await
                .unwrap(),
            "a caller who can read nothing is answered without a query"
        );
        assert!(
            !notifications(&store)
                .attachment_is_visible(&AttachmentId::generate(), std::slice::from_ref(&channel))
                .await
                .unwrap(),
            "an id nothing references is not visible anywhere"
        );

        // And it stops being visible the moment the last notification showing it is
        // gone — the same instant the sweep may collect the object.
        notifications(&store)
            .mark_deleted(shown.id, now())
            .await
            .unwrap();
        assert!(
            !notifications(&store)
                .attachment_is_visible(&reference, &[channel])
                .await
                .unwrap(),
            "a deleted notification stops entitling anybody to its image"
        );
    }

    #[tokio::test]
    async fn forgetting_nothing_is_not_a_query() {
        let store = store!();
        assert_eq!(registry(&store).forget(&[]).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn nothing_expiring_means_nothing_to_collect() {
        // Guards the `NOT IN` trap the query deliberately avoids: over a set that
        // contains a NULL, `NOT IN` is never true, and this call would return an
        // empty list forever while looking exactly like a clean bucket.
        let store = store!();
        let (_, alice) = user();
        let channel = seeded_channel(&store, &alice).await;
        let reference = AttachmentId::generate();

        publish_with(
            &store,
            &channel,
            &alice,
            RevisionDraft::lifecycle(
                State::Open,
                ContentDraft {
                    image: Some(Image::Attachment { reference }),
                    ..ContentDraft::titled("current")
                },
            ),
            None,
            now(),
        )
        .await;

        let orphaned = store
            .orphaned_attachments(now() - Duration::days(7), 100)
            .await
            .unwrap();

        assert!(
            !orphaned.contains(&reference),
            "nothing was deleted, so nothing is collectable"
        );
    }

    #[tokio::test]
    async fn a_tombstoned_row_only_goes_once_its_grace_period_has_passed() {
        let store = store!();
        let (_, alice) = user();
        let channel = seeded_channel(&store, &alice).await;
        let (published, _) = publish(&store, &channel, &alice, "will be purged").await;

        store
            .mark_deleted(published.id, now() - Duration::days(10))
            .await
            .unwrap();

        let too_early = store.purge(now() - Duration::days(30), 100).await.unwrap();
        assert_eq!(
            too_early, 0,
            "a client that was offline still needs the tombstone"
        );

        let purged = store.purge(now() - Duration::days(7), 100).await.unwrap();
        assert!(purged >= 1);
        assert!(
            NotificationRepository::get(&store, published.id)
                .await
                .unwrap()
                .is_none()
        );
    }

    // --- Channels ----------------------------------------------------------

    #[tokio::test]
    async fn a_channel_round_trips_with_its_members_and_policy() {
        let store = store!();
        let (_, alice) = user();
        let (_, bob) = user();

        let id = fresh_channel();
        let mut channel = Channel::dynamic(id.clone(), Some("CI infra")).unwrap();
        channel.upsert_member(
            Membership::new(alice.clone(), vec![Scope::Publish, Scope::Subscribe]).unwrap(),
        );
        channel.upsert_member(Membership::new(bob.clone(), vec![Scope::Subscribe]).unwrap());
        channels(&store).upsert(&channel).await.unwrap();

        let found = ChannelRepository::get(&store, &id)
            .await
            .unwrap()
            .expect("it was just written");

        assert_eq!(found.display_name.as_deref(), Some("CI infra"));
        assert_eq!(found.members.len(), 2);
        assert!(found.grants(&alice, Scope::Publish));
        assert!(!found.grants(&bob, Scope::Publish));
        assert_eq!(found.policy, ChannelPolicy::default());
    }

    #[tokio::test]
    async fn upserting_a_channel_replaces_its_members_rather_than_adding_to_them() {
        let store = store!();
        let (_, alice) = user();
        let (_, bob) = user();
        let id = seeded_channel(&store, &alice).await;

        let mut channel = ChannelRepository::get(&store, &id).await.unwrap().unwrap();
        channel.members.clear();
        channel.upsert_member(Membership::new(bob.clone(), vec![Scope::Subscribe]).unwrap());
        channels(&store).upsert(&channel).await.unwrap();

        let found = ChannelRepository::get(&store, &id).await.unwrap().unwrap();

        assert_eq!(found.members.len(), 1);
        assert!(
            !found.grants(&alice, Scope::Subscribe),
            "removing somebody has to actually remove them"
        );
    }

    #[tokio::test]
    async fn membership_is_the_only_door_to_a_channel() {
        let store = store!();
        let (_, alice) = user();
        let (_, bob) = user();
        let mine = seeded_channel(&store, &alice).await;
        seeded_channel(&store, &bob).await;

        let subscribed = channels(&store).subscribed_ids(&alice, &[]).await.unwrap();
        assert_eq!(subscribed, vec![mine.clone()]);

        let listed = channels(&store)
            .list_for(&alice, &[], Scope::Publish, PageRequest::first())
            .await
            .unwrap();
        assert_eq!(listed.items.len(), 1);
        assert_eq!(listed.items[0].id, mine);

        let nothing = channels(&store)
            .list_for(&bob, &[], Scope::Publish, PageRequest::first())
            .await
            .unwrap();
        assert_eq!(nothing.items.len(), 1, "bob has his own, and only his own");
        assert_ne!(nothing.items[0].id, mine);
    }

    #[tokio::test]
    async fn a_group_binding_scopes_a_caller_who_is_not_a_member_of_anything() {
        let store = store!();
        let (alice_id, alice) = user();
        let (_, carol) = user();
        let group = fresh_group();

        // Nobody is a named member. Access comes entirely from the token's groups.
        let id = fresh_channel();
        let mut channel = Channel::dynamic(id.clone(), None).unwrap();
        channel.upsert_group(
            GroupBinding::new(group.clone(), vec![Scope::Publish, Scope::Subscribe]).unwrap(),
        );
        channels(&store).upsert(&channel).await.unwrap();

        let infra = vec![group.clone()];

        let stored = ChannelRepository::get(&store, &id).await.unwrap().unwrap();
        assert_eq!(stored.group_access.len(), 1);
        assert_eq!(
            stored.group_access[0].scopes,
            [Scope::Publish, Scope::Subscribe]
        );

        assert_eq!(
            channels(&store)
                .subscribed_ids(&alice, &infra)
                .await
                .unwrap(),
            vec![id.clone()]
        );
        assert!(
            channels(&store)
                .subscribed_ids(&carol, &[])
                .await
                .unwrap()
                .is_empty(),
            "T29: no groups means individual bindings only, never a default grant"
        );

        let listed = channels(&store)
            .list_for(&alice, &infra, Scope::Publish, PageRequest::first())
            .await
            .unwrap();
        assert_eq!(listed.items.len(), 1);
        assert!(
            channels(&store)
                .list_for(&alice, &infra, Scope::Manage, PageRequest::first())
                .await
                .unwrap()
                .is_empty(),
            "a binding grants its scopes and no others"
        );

        // And the backlog follows the same door, or a group-based caller would
        // reconnect to an empty stream.
        publish(&store, &id, &alice, "for the group").await;
        let page = events(&store)
            .sync(
                alice_id,
                &SyncQuery {
                    after: None,
                    channels: vec![id],
                    groups: infra,
                    page: PageRequest::first(),
                },
            )
            .await
            .unwrap();
        assert_eq!(page.events.len(), 1);
    }

    #[tokio::test]
    async fn a_caller_who_is_both_a_member_and_in_a_bound_group_gets_the_channel_once() {
        let store = store!();
        let (_, alice) = user();
        let group = fresh_group();

        let id = fresh_channel();
        let mut channel = Channel::dynamic(id.clone(), None).unwrap();
        channel.upsert_member(Membership::new(alice.clone(), vec![Scope::Subscribe]).unwrap());
        channel.upsert_group(GroupBinding::new(group.clone(), vec![Scope::Subscribe]).unwrap());
        channels(&store).upsert(&channel).await.unwrap();

        let scoped = channels(&store)
            .subscribed_ids(&alice, &[group])
            .await
            .unwrap();

        assert_eq!(
            scoped,
            vec![id],
            "a JOIN would return it twice, and the pagination cursor would skip a row"
        );
    }

    #[tokio::test]
    async fn removing_a_group_binding_actually_removes_the_access() {
        let store = store!();
        let (_, alice) = user();
        let group = fresh_group();

        let id = fresh_channel();
        let mut channel = Channel::dynamic(id.clone(), None).unwrap();
        channel.upsert_group(GroupBinding::new(group.clone(), vec![Scope::Subscribe]).unwrap());
        channels(&store).upsert(&channel).await.unwrap();

        channel.group_access.clear();
        channels(&store).upsert(&channel).await.unwrap();

        assert!(
            channels(&store)
                .subscribed_ids(&alice, &[group])
                .await
                .unwrap()
                .is_empty(),
            "bindings are replaced wholesale, like members"
        );
    }

    #[tokio::test]
    async fn a_managed_channel_is_listed_for_the_reconciler_and_a_dynamic_one_is_not() {
        let store = store!();
        let (_, alice) = user();

        let dynamic = seeded_channel(&store, &alice).await;
        let managed_id = fresh_channel();
        let mut managed = Channel::dynamic(managed_id.clone(), None).unwrap();
        managed.origin = notify_domain::Origin::Managed;
        channels(&store).upsert(&managed).await.unwrap();

        let listed = channels(&store).list_managed().await.unwrap();
        let ids: Vec<_> = listed.iter().map(|channel| channel.id.clone()).collect();

        assert!(ids.contains(&managed_id));
        assert!(!ids.contains(&dynamic));

        assert!(
            ChannelRepository::delete(&store, &managed_id)
                .await
                .unwrap()
        );
        assert!(
            !ChannelRepository::delete(&store, &managed_id)
                .await
                .unwrap(),
            "deleting says whether there was anything there"
        );
    }

    // --- Identities --------------------------------------------------------

    #[tokio::test]
    async fn a_user_is_found_by_every_handle_the_config_can_bind_on() {
        let store = store!();
        let subject = format!("oidc-{}", ulid_slug());
        let email = format!("{}@weebo.local", ulid_slug());

        let mut user = User::from_claims(
            UserId::generate(),
            &subject,
            Some(&email),
            Some("Alice Martin"),
            now(),
        )
        .unwrap();
        // A claimed field and a timezone, so the round trip covers the columns the
        // profile added rather than only the ones that were already there (§2.4).
        user.edit_profile(notify_domain::ProfileEdit {
            display_name: Some(notify_domain::FieldEdit::Set("alice".to_owned())),
            timezone: Some(notify_domain::FieldEdit::Set("Europe/Paris".to_owned())),
            ..notify_domain::ProfileEdit::default()
        })
        .unwrap();

        users(&store).upsert(&user).await.unwrap();

        let stored = UserRepository::get(&store, user.id).await.unwrap().unwrap();
        assert_eq!(stored.id, user.id);
        // The profile has to survive the round trip whole, override set included:
        // losing that set is losing the feature, and it would look exactly like the
        // identity provider quietly winning again (§2.4).
        assert_eq!(stored.profile, user.profile);
        assert_eq!(stored.profile.timezone.as_deref(), Some("Europe/Paris"));
        assert!(
            stored
                .profile
                .is_overridden(notify_domain::ProfileField::DisplayName)
        );

        assert_eq!(
            store.find_by_subject(&subject).await.unwrap().unwrap().id,
            user.id
        );
        // Case-insensitive, matching the index: a provider that changes the
        // capitalisation of an address must not orphan an individual binding.
        assert_eq!(
            store
                .find_by_email(&email.to_uppercase())
                .await
                .unwrap()
                .unwrap()
                .id,
            user.id
        );
        assert!(store.find_by_subject("nobody").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_service_account_round_trips_and_reports_its_origin() {
        let store = store!();
        let id = ServiceAccountId::parse(&format!("bot-{}", ulid_slug())).unwrap();

        let account = ServiceAccount {
            id: id.clone(),
            display_name: Some("ArgoCD".to_owned()),
            description: Some("Publishes the state of ArgoCD syncs".to_owned()),
            origin: notify_domain::Origin::Managed,
            created_by: None,
            created_at: now(),
            disabled_at: None,
        };
        accounts(&store).upsert(&account).await.unwrap();

        let found = ServiceAccountRepository::get(&store, &id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.display_name.as_deref(), Some("ArgoCD"));

        assert!(
            accounts(&store)
                .list_managed()
                .await
                .unwrap()
                .iter()
                .any(|candidate| candidate.id == id)
        );
        assert!(
            ServiceAccountRepository::list(&store)
                .await
                .unwrap()
                .iter()
                .any(|candidate| candidate.id == id)
        );
    }

    #[tokio::test]
    async fn an_api_key_authenticates_by_prefix_and_dies_on_revocation() {
        let store = store!();
        let (alice_id, alice) = user();
        let prefix = ApiKeyPrefix::parse(fresh_prefix().as_str()).unwrap();

        let key = ApiKey {
            id: ApiKeyId::generate(),
            prefix: prefix.clone(),
            label: Some("ci".to_owned()),
            owner: ApiKeyOwner::User { id: alice_id },
            scopes: vec![Scope::Publish],
            channels: Vec::new(),
            publish_per_minute: None,
            created_at: now(),
            expires_at: None,
            last_used_at: None,
            revoked_at: None,
        };
        keys(&store)
            .insert(&StoredApiKey {
                key: key.clone(),
                secret_hash: "$argon2id$stub".to_owned(),
            })
            .await
            .unwrap();

        let stored = store.find_by_prefix(&prefix).await.unwrap().unwrap();
        assert_eq!(stored.key.id, key.id);
        assert_eq!(
            stored.secret_hash, "$argon2id$stub",
            "the hash exists here and nowhere else"
        );

        assert_eq!(
            ApiKeyRepository::get(&store, key.id)
                .await
                .unwrap()
                .unwrap()
                .id,
            key.id
        );
        assert_eq!(keys(&store).list_for(&alice).await.unwrap().len(), 1);

        store.touch(key.id, now()).await.unwrap();
        assert!(
            ApiKeyRepository::get(&store, key.id)
                .await
                .unwrap()
                .unwrap()
                .last_used_at
                .is_some()
        );

        assert!(keys(&store).revoke(key.id, now()).await.unwrap());
        assert!(
            !keys(&store).revoke(key.id, now()).await.unwrap(),
            "revoking twice reports that there was nothing left to revoke"
        );
    }

    #[tokio::test]
    async fn a_platform_admins_ceiling_on_a_key_round_trips_through_a_widened_column() {
        // D24: the domain value is `u32`, the column is `BIGINT` precisely so this
        // never has to refuse a value it wrote itself — proved here with a real
        // value near the top of `u32`'s range, not just a small round number.
        let store = store!();
        let (alice_id, _alice) = user();
        let key = ApiKey {
            id: ApiKeyId::generate(),
            prefix: ApiKeyPrefix::parse(fresh_prefix().as_str()).unwrap(),
            label: None,
            owner: ApiKeyOwner::User { id: alice_id },
            scopes: vec![Scope::Publish],
            channels: Vec::new(),
            publish_per_minute: None,
            created_at: now(),
            expires_at: None,
            last_used_at: None,
            revoked_at: None,
        };
        keys(&store)
            .insert(&StoredApiKey {
                key: key.clone(),
                secret_hash: "$argon2id$stub".to_owned(),
            })
            .await
            .unwrap();

        keys(&store)
            .set_publish_limit(key.id, Some(u32::MAX - 1))
            .await
            .unwrap();
        assert_eq!(
            ApiKeyRepository::get(&store, key.id)
                .await
                .unwrap()
                .unwrap()
                .publish_per_minute,
            Some(u32::MAX - 1)
        );

        keys(&store).set_publish_limit(key.id, None).await.unwrap();
        assert_eq!(
            ApiKeyRepository::get(&store, key.id)
                .await
                .unwrap()
                .unwrap()
                .publish_per_minute,
            None,
            "clearing it is the same operation as setting one, and it round-trips too"
        );
    }

    #[tokio::test]
    async fn critical_t25_disabling_a_service_account_takes_every_key_with_it() {
        let store = store!();
        let account = ServiceAccountId::parse(&format!("bot-{}", ulid_slug())).unwrap();
        let principal = Principal::ServiceAccount {
            id: account.clone(),
        };

        for _ in 0..3 {
            let key = ApiKey {
                id: ApiKeyId::generate(),
                prefix: ApiKeyPrefix::parse(fresh_prefix().as_str()).unwrap(),
                label: None,
                owner: ApiKeyOwner::ServiceAccount {
                    id: account.clone(),
                },
                scopes: vec![Scope::Publish],
                channels: Vec::new(),
                publish_per_minute: None,
                created_at: now(),
                expires_at: None,
                last_used_at: None,
                revoked_at: None,
            };
            keys(&store)
                .insert(&StoredApiKey {
                    key,
                    secret_hash: "$argon2id$stub".to_owned(),
                })
                .await
                .unwrap();
        }

        let revoked = store.revoke_all_for(&principal, now()).await.unwrap();

        assert_eq!(
            revoked, 3,
            "one statement, so nothing created mid-sweep survives"
        );
        assert!(
            keys(&store)
                .list_for(&principal)
                .await
                .unwrap()
                .iter()
                .all(|key| key.revoked_at.is_some())
        );
    }

    // --- Devices -----------------------------------------------------------

    #[tokio::test]
    async fn a_device_round_trips_with_its_transport_and_key() {
        let store = store!();
        let (_, alice) = user();

        let device = Device {
            id: DeviceId::generate(),
            owner: alice.clone(),
            label: "Pixel".to_owned(),
            transport: PushTransport::UnifiedPush {
                endpoint: "https://push.weebo.local/abc".parse().unwrap(),
            },
            public_key: Some(DevicePublicKey::from_bytes([7_u8; 32])),
            cursor: None,
            enrolled_at: now(),
            last_seen_at: None,
            revoked_at: None,
        };
        devices(&store).upsert(&device).await.unwrap();

        let found = DeviceRepository::get(&store, device.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.label, "Pixel");
        assert_eq!(found.public_key, device.public_key);
        assert_eq!(found.transport, device.transport);
        assert_eq!(
            DeviceRepository::list_for(&store, &alice)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn critical_t18_revoking_a_device_takes_its_key_and_its_subscription() {
        let store = store!();
        let (_, alice) = user();

        let device = Device {
            id: DeviceId::generate(),
            owner: alice.clone(),
            label: "lost phone".to_owned(),
            transport: PushTransport::UnifiedPush {
                endpoint: "https://push.weebo.local/abc".parse().unwrap(),
            },
            public_key: Some(DevicePublicKey::from_bytes([7_u8; 32])),
            cursor: Some(Cursor::from_sequence(9)),
            enrolled_at: now(),
            last_seen_at: None,
            revoked_at: None,
        };
        devices(&store).upsert(&device).await.unwrap();

        assert!(devices(&store).revoke(device.id, now()).await.unwrap());

        let found = DeviceRepository::get(&store, device.id)
            .await
            .unwrap()
            .unwrap();
        assert!(found.revoked_at.is_some());
        assert!(
            found.public_key.is_none(),
            "a sealed box must not stay addressable to a device nobody controls"
        );
        assert_eq!(found.transport, PushTransport::Stream);
        assert!(found.cursor.is_none());
        assert!(
            !devices(&store).revoke(device.id, now()).await.unwrap(),
            "revoking twice reports there was nothing left"
        );
    }

    #[tokio::test]
    async fn a_device_cursor_never_goes_backwards() {
        let store = store!();
        let (_, alice) = user();

        let device = Device {
            id: DeviceId::generate(),
            owner: alice,
            label: "laptop".to_owned(),
            transport: PushTransport::Stream,
            public_key: None,
            cursor: None,
            enrolled_at: now(),
            last_seen_at: None,
            revoked_at: None,
        };
        devices(&store).upsert(&device).await.unwrap();

        store
            .advance_cursor(device.id, Cursor::from_sequence(20), now())
            .await
            .unwrap();
        // Two tabs of the same client: the older acknowledgement must not win and
        // re-deliver what was already seen.
        store
            .advance_cursor(device.id, Cursor::from_sequence(10), now())
            .await
            .unwrap();

        let found = DeviceRepository::get(&store, device.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.cursor, Some(Cursor::from_sequence(20)));
    }

    #[tokio::test]
    async fn delivery_targets_are_the_active_devices_of_the_subscribers() {
        let store = store!();
        let (_, alice) = user();
        let (_, bob) = user();
        let channel = seeded_channel(&store, &alice).await;

        let mut device = Device {
            id: DeviceId::generate(),
            owner: alice.clone(),
            label: "alice's".to_owned(),
            transport: PushTransport::Stream,
            public_key: None,
            cursor: None,
            enrolled_at: now(),
            last_seen_at: None,
            revoked_at: None,
        };
        devices(&store).upsert(&device).await.unwrap();

        // Bob is not a member, so his device is not a target however active it is.
        device.id = DeviceId::generate();
        device.owner = bob;
        device.label = "bob's".to_owned();
        devices(&store).upsert(&device).await.unwrap();

        let targets = store.delivery_targets(&channel).await.unwrap();

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].label, "alice's");
    }

    #[tokio::test]
    async fn delivery_targets_reach_a_group_bound_subscriber_once_their_groups_are_remembered() {
        let store = store!();
        let (_, alice) = user();
        let group = fresh_group();

        // Alice is not a named member anywhere — her only access is the group.
        let id = fresh_channel();
        let mut channel = Channel::dynamic(id.clone(), None).unwrap();
        channel.upsert_group(GroupBinding::new(group.clone(), vec![Scope::Subscribe]).unwrap());
        channels(&store).upsert(&channel).await.unwrap();

        let device = Device {
            id: DeviceId::generate(),
            owner: alice.clone(),
            label: "alice's".to_owned(),
            transport: PushTransport::Stream,
            public_key: None,
            cursor: None,
            enrolled_at: now(),
            last_seen_at: None,
            revoked_at: None,
        };
        devices(&store).upsert(&device).await.unwrap();

        assert!(
            store.delivery_targets(&id).await.unwrap().is_empty(),
            "no snapshot yet: nothing this port persists ties alice to the group"
        );

        devices(&store)
            .remember_groups(&alice, &[group], now())
            .await
            .unwrap();

        let targets = store.delivery_targets(&id).await.unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].label, "alice's");

        // A later snapshot with no matching group reads as the group being gone —
        // exactly the "loses access on their next token" promise (§11), applied to
        // whatever the last remembered token said instead of a live one.
        devices(&store)
            .remember_groups(&alice, &[fresh_group()], now())
            .await
            .unwrap();
        assert!(store.delivery_targets(&id).await.unwrap().is_empty());
    }

    // --- What GitOps last applied (design §11, §13, D28) ---------------------

    #[tokio::test]
    async fn a_declaration_set_is_replaced_and_not_merged() {
        let store = store!();

        // Namespaced per run: this suite shares one database and deliberately
        // leaves its rows behind.
        //
        // **This test is more invasive than the rest of the suite**, and it is worth
        // knowing before running it against a database somebody is using: `apply`
        // replaces a whole *kind* by design — that is what makes a withdrawn policy
        // stop being stored — so it deletes whatever the running server last wrote.
        // Nothing is lost, since the reconciler rebuilds the snapshot on its next
        // pass, but that pass reports the difference: a handful of `added` and
        // `removed` entries appear in the audit log with no configuration change
        // behind them. Harmless in a test database, confusing in a shared one.
        let kind = DeclarationKind::TrustPolicy;
        let unique = notify_domain::ReceiptId::generate().to_string();
        let key = |suffix: &str| format!("{unique}-{suffix}");

        let first = vec![
            Declaration {
                key: key("kept"),
                snapshot: serde_json::json!({ "channels": [] }),
            },
            Declaration {
                key: key("gone"),
                snapshot: serde_json::json!({ "channels": ["a"] }),
            },
        ];
        declarations(&store)
            .apply(kind, &first, now())
            .await
            .unwrap();

        let stored = declarations(&store).snapshot(kind).await.unwrap();
        let mine: Vec<_> = stored
            .iter()
            .filter(|d| d.key.starts_with(&unique))
            .collect();
        assert_eq!(mine.len(), 2);

        let second = vec![Declaration {
            key: key("kept"),
            snapshot: serde_json::json!({ "channels": ["ci-infra"] }),
        }];
        declarations(&store)
            .apply(kind, &second, now())
            .await
            .unwrap();

        let stored = declarations(&store).snapshot(kind).await.unwrap();
        let mine: Vec<_> = stored
            .iter()
            .filter(|d| d.key.starts_with(&unique))
            .collect();

        // The half an upsert would lose. A withdrawn trust policy has to stop being
        // stored, or the next pass reports it as still present for ever.
        assert_eq!(mine.len(), 1, "a withdrawn declaration must not survive");
        assert_eq!(mine[0].key, key("kept"));
        assert_eq!(
            mine[0].snapshot,
            serde_json::json!({ "channels": ["ci-infra"] }),
            "and the one that stayed carries its new body, not its old one"
        );
    }

    #[tokio::test]
    async fn the_configuration_can_be_an_audit_actor() {
        let store = store!();
        let channel = seeded_channel(&store, &user().1).await;

        audit(&store)
            .record(&AuditEntry {
                actor: notify_ports::AuditActor::configuration(),
                action: notify_ports::actions::TRUST_POLICY_ADDED,
                target: Some("https://forge.example repo:a".to_owned()),
                channel: Some(channel.clone()),
                detail: Some(serde_json::json!({ "declaration": { "channels": [] } })),
                at: now(),
            })
            .await
            .unwrap();

        // The round trip is the point: D28 had no emitter partly because there was
        // no honest value for this field, and a value that can be written but not
        // read back is what broke `GET /admin/audit` once already (M4duovicies).
        let back = audit(&store)
            .query(&AuditQuery {
                actor: Some(notify_ports::AuditActor::configuration()),
                channel: Some(channel),
                ..AuditQuery::default()
            })
            .await
            .unwrap();

        assert_eq!(back.items.len(), 1);
        assert_eq!(
            back.items[0].actor,
            notify_ports::AuditActor::configuration()
        );
        assert_eq!(back.items[0].action.as_str(), "trust_policy.added");
    }

    // --- Receipts and interactions -----------------------------------------

    #[tokio::test]
    async fn a_receipt_is_recorded_once_however_often_it_is_retried() {
        let store = store!();
        let (_, alice) = user();
        let channel = seeded_channel(&store, &alice).await;
        let (published, _) = publish(&store, &channel, &alice, "seen by").await;

        let receipt = Receipt {
            id: notify_domain::ReceiptId::generate(),
            notification_id: published.id,
            principal: alice.clone(),
            device: None,
            kind: ReceiptKind::Seen,
            at: now(),
        };

        assert!(
            receipts(&store)
                .record(&receipt, Visibility::Channel)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            receipts(&store)
                .record(&receipt, Visibility::Channel)
                .await
                .unwrap()
                .is_none(),
            "a retried acknowledgement must not inflate `seen by 3/5`"
        );

        assert_eq!(
            ReceiptRepository::list_for(&store, published.id)
                .await
                .unwrap()
                .len(),
            1
        );
        // The badge's question, asked the way D27 settled it: one aggregate over the
        // channel, not a predicate per notification. Alice has read the only row, so
        // her channel is *absent* rather than present with a zero — the port promises
        // one representation of "no badge" instead of two.
        assert!(
            notifications(&store)
                .unread_counts(&alice, std::slice::from_ref(&channel))
                .await
                .unwrap()
                .is_empty()
        );

        let (_, bob) = user();
        assert_eq!(
            notifications(&store)
                .unread_counts(&bob, std::slice::from_ref(&channel))
                .await
                .unwrap(),
            vec![UnreadCount {
                channel: channel.clone(),
                unread: 1,
            }],
            "a receipt is one principal's, and must not clear anybody else's badge"
        );
    }

    #[tokio::test]
    async fn an_acknowledgement_is_idempotent_and_a_pick_replaces_the_previous_one() {
        let store = store!();
        let (_, alice) = user();
        let channel = seeded_channel(&store, &alice).await;

        let actions = notify_domain::Actions::new(vec![
            notify_domain::ActionDraft::Ack {
                key: "ack-incident".to_owned(),
                label: "Acknowledge".to_owned(),
            }
            .try_into()
            .unwrap(),
        ])
        .unwrap();

        let (published, _) = publish_with(
            &store,
            &channel,
            &alice,
            RevisionDraft::lifecycle(
                State::Open,
                ContentDraft {
                    actions: actions.clone(),
                    ..ContentDraft::titled("needs an ack")
                },
            ),
            None,
            now(),
        )
        .await;

        let mut log = InteractionLog::default();
        let key = notify_domain::ActionKey::parse("ack-incident").unwrap();

        let first = log
            .record(
                &published.head.content.actions,
                published.id,
                author(&alice),
                Interaction::Ack { key: key.clone() },
                now(),
            )
            .unwrap();
        assert!(
            InteractionRepository::apply(&store, &first)
                .await
                .unwrap()
                .is_some()
        );

        let again = log
            .record(
                &published.head.content.actions,
                published.id,
                author(&alice),
                Interaction::Ack { key },
                now(),
            )
            .unwrap();
        assert!(
            InteractionRepository::apply(&store, &again)
                .await
                .unwrap()
                .is_none(),
            "`Unchanged` writes nothing and broadcasts nothing"
        );

        assert_eq!(
            InteractionRepository::list_for(&store, published.id)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn critical_t26_a_silence_is_announced_to_its_own_user_and_to_nobody_else() {
        let store = store!();
        let (alice_id, alice) = user();
        let (bob_id, bob) = user();

        // Both in the same channel: sharing one must not make alice's silences bob's
        // business.
        let channel = fresh_channel();
        let mut shared = Channel::dynamic(channel.clone(), None).unwrap();
        shared.upsert_member(Membership::new(alice.clone(), vec![Scope::Subscribe]).unwrap());
        shared.upsert_member(Membership::new(bob.clone(), vec![Scope::Subscribe]).unwrap());
        channels(&store).upsert(&shared).await.unwrap();

        let silence = Silence {
            user: alice_id,
            target: SilenceTarget::Group {
                key: "release-9".to_owned(),
            },
            until: Some(now() + Duration::hours(2)),
            since: now(),
        };
        let cursor = store.silence(&silence).await.unwrap();

        let hers = store
            .sync(
                alice_id,
                &SyncQuery {
                    after: Some(Cursor::from_sequence(cursor.sequence() - 1)),
                    channels: Vec::new(),
                    groups: Vec::new(),
                    page: PageRequest::first(),
                },
            )
            .await
            .unwrap();
        assert!(
            hers.events
                .iter()
                .any(|event| matches!(&event.payload, EventPayload::Preference(_))),
            "she has to learn her own preferences changed"
        );

        let his = store
            .sync(
                bob_id,
                &SyncQuery {
                    after: Some(Cursor::from_sequence(cursor.sequence() - 1)),
                    channels: Vec::new(),
                    groups: Vec::new(),
                    page: PageRequest::first(),
                },
            )
            .await
            .unwrap();
        assert!(
            !his.events
                .iter()
                .any(|event| matches!(&event.payload, EventPayload::Preference(_))),
            "sharing a channel with somebody does not make their silences your business"
        );

        let active = store.active_silences(alice_id, now()).await.unwrap();
        assert_eq!(active.len(), 1);

        // Re-silencing the same target extends it rather than stacking a second one
        // nobody can see to lift.
        store.silence(&silence).await.unwrap();
        assert_eq!(
            store.active_silences(alice_id, now()).await.unwrap().len(),
            1
        );

        let lapsed = store
            .active_silences(alice_id, now() + Duration::hours(3))
            .await
            .unwrap();
        assert!(lapsed.is_empty());
    }

    // --- Do not disturb (design §7.4) --------------------------------------

    #[tokio::test]
    async fn a_preference_nobody_set_reads_as_one_that_mutes_nothing() {
        let store = store!();
        let (alice_id, _) = user();

        // No row, and no error: "never opened Settings" and "a preference that
        // mutes nothing" are the same answer, so there is no absence to handle.
        let dnd = PreferenceRepository::dnd(&store, alice_id).await.unwrap();

        assert_eq!(dnd.user, alice_id);
        assert!(dnd.quiet_hours.is_empty());
        assert!(!dnd.is_active(now()));
    }

    #[tokio::test]
    async fn a_preference_survives_the_round_trip_through_jsonb() {
        let store = store!();
        let (alice_id, _) = user();
        let channel = fresh_channel();

        let wanted = Dnd {
            quiet_hours: vec![
                QuietHours::new(
                    vec![Day::Fri, Day::Sat],
                    WallClock::new(22, 0).unwrap(),
                    WallClock::new(8, 0).unwrap(),
                    "Europe/Paris",
                )
                .unwrap(),
            ],
            until: Some(now() + Duration::hours(2)),
            scope: DndScope::Channels {
                channels: vec![channel.clone()],
            },
            exceptions: vec![channel],
            urgent_bypasses: true,
            ..Dnd::off(alice_id)
        };

        store.set_dnd(&wanted, now()).await.unwrap();

        assert_eq!(
            PreferenceRepository::dnd(&store, alice_id).await.unwrap(),
            wanted,
            "the whole document comes back, not the fields this query happened to name"
        );
    }

    #[tokio::test]
    async fn setting_one_replaces_the_previous_rather_than_stacking() {
        let store = store!();
        let (alice_id, _) = user();

        store
            .set_dnd(
                &Dnd {
                    until: Some(now() + Duration::hours(2)),
                    ..Dnd::off(alice_id)
                },
                now(),
            )
            .await
            .unwrap();
        store.set_dnd(&Dnd::off(alice_id), now()).await.unwrap();

        assert!(
            !PreferenceRepository::dnd(&store, alice_id)
                .await
                .unwrap()
                .is_active(now()),
            "turning it off has to actually turn it off"
        );
    }

    #[tokio::test]
    async fn a_held_back_push_is_counted_and_the_failures_among_them_separately() {
        let store = store!();
        let (alice_id, _) = user();
        store.set_dnd(&Dnd::off(alice_id), now()).await.unwrap();

        for status in [Status::Info, Status::Failure, Status::Success] {
            PreferenceRepository::defer(&store, alice_id, status)
                .await
                .unwrap();
        }

        let mine = store
            .deferred_tallies(100)
            .await
            .unwrap()
            .into_iter()
            .find(|tally| tally.dnd.user == alice_id)
            .expect("a tally with something on it comes back");

        assert_eq!(mine.total, 3);
        assert_eq!(mine.failures, 1, "the one status a digest calls out");

        PreferenceRepository::clear_deferred(&store, alice_id)
            .await
            .unwrap();
        assert!(
            !store
                .deferred_tallies(100)
                .await
                .unwrap()
                .iter()
                .any(|tally| tally.dnd.user == alice_id),
            "a cleared tally is gone, so the digest goes out once"
        );
    }

    #[tokio::test]
    async fn somebody_with_no_preference_cannot_have_anything_held_back() {
        let store = store!();
        let (alice_id, _) = user();

        // No row: nothing could have muted them, so the statement matching nothing
        // is the right outcome rather than something to report.
        PreferenceRepository::defer(&store, alice_id, Status::Failure)
            .await
            .unwrap();

        assert!(
            !store
                .deferred_tallies(100)
                .await
                .unwrap()
                .iter()
                .any(|tally| tally.dnd.user == alice_id)
        );
    }

    #[tokio::test]
    async fn replacing_a_preference_drops_what_was_counted_under_the_old_one() {
        let store = store!();
        let (alice_id, _) = user();
        store.set_dnd(&Dnd::off(alice_id), now()).await.unwrap();
        PreferenceRepository::defer(&store, alice_id, Status::Failure)
            .await
            .unwrap();

        // A digest counting a quiet period somebody has just redefined would be
        // reporting something that, under the new preference, did not happen.
        store
            .set_dnd(
                &Dnd {
                    until: Some(now() + Duration::hours(1)),
                    ..Dnd::off(alice_id)
                },
                now(),
            )
            .await
            .unwrap();

        assert!(
            !store
                .deferred_tallies(100)
                .await
                .unwrap()
                .iter()
                .any(|tally| tally.dnd.user == alice_id)
        );
    }

    /// The reason both writers go through one builder: a `silence` must not ship a
    /// `preference` event that has forgotten the do-not-disturb it did not touch.
    #[tokio::test]
    async fn critical_t26_a_preference_event_carries_both_halves_whichever_one_changed() {
        let store = store!();
        let (alice_id, alice) = user();

        let channel = fresh_channel();
        let mut shared = Channel::dynamic(channel.clone(), None).unwrap();
        shared.upsert_member(Membership::new(alice.clone(), vec![Scope::Subscribe]).unwrap());
        channels(&store).upsert(&shared).await.unwrap();

        store
            .set_dnd(
                &Dnd {
                    exceptions: vec![channel],
                    ..Dnd::off(alice_id)
                },
                now(),
            )
            .await
            .unwrap();

        // A silence, which writes the *other* half.
        let cursor = store
            .silence(&Silence {
                user: alice_id,
                target: SilenceTarget::Group {
                    key: "release-9".to_owned(),
                },
                until: Some(now() + Duration::hours(2)),
                since: now(),
            })
            .await
            .unwrap();

        let page = store
            .sync(
                alice_id,
                &SyncQuery {
                    after: Some(Cursor::from_sequence(cursor.sequence() - 1)),
                    channels: Vec::new(),
                    groups: Vec::new(),
                    page: PageRequest::first(),
                },
            )
            .await
            .unwrap();

        let preference = page
            .events
            .iter()
            .find_map(|event| match &event.payload {
                EventPayload::Preference(preference) => Some(preference),
                _ => None,
            })
            .expect("the silence announced itself");

        assert_eq!(preference.silences.len(), 1);
        assert_eq!(
            preference.dnd.exceptions.len(),
            1,
            "the half this write did not touch still has to be on the wire"
        );
    }

    // --- The event log -----------------------------------------------------

    #[tokio::test]
    async fn critical_t7_a_sync_replays_exactly_what_was_missed() {
        let store = store!();
        let (alice_id, alice) = user();
        let channel = seeded_channel(&store, &alice).await;

        let head = EventStore::head(&store).await.unwrap();

        for index in 0..3 {
            publish(&store, &channel, &alice, &format!("entry {index}")).await;
        }

        let page = store
            .sync(
                alice_id,
                &SyncQuery {
                    after: Some(head),
                    channels: vec![channel.clone()],
                    groups: Vec::new(),
                    page: PageRequest::first(),
                },
            )
            .await
            .unwrap();

        assert_eq!(page.events.len(), 3);
        assert!(
            page.events
                .windows(2)
                .all(|pair| pair[0].cursor < pair[1].cursor),
            "no gaps, no duplicates, and in order"
        );

        // Resuming from the last cursor returns nothing: caught up is caught up.
        let last = page.events.last().unwrap().cursor;
        let nothing = store
            .sync(
                alice_id,
                &SyncQuery {
                    after: Some(last),
                    channels: vec![channel],
                    groups: Vec::new(),
                    page: PageRequest::first(),
                },
            )
            .await
            .unwrap();
        assert!(nothing.events.is_empty());
        assert!(nothing.next.is_none());
    }

    #[tokio::test]
    async fn compaction_folds_a_noisy_deploy_into_one_entry_and_says_how_many() {
        let store = store!();
        let (alice_id, alice) = user();
        let channel = seeded_channel(&store, &alice).await;
        let head = EventStore::head(&store).await.unwrap();

        let (mut published, _) = publish(&store, &channel, &alice, "deploy prod").await;
        for step in 0..5 {
            let written = published
                .revise(
                    RevisionDraft::simple(ContentDraft::titled(&format!("step {step}"))),
                    author(&alice),
                    now(),
                    &ChannelPolicy::default(),
                )
                .unwrap();
            store
                .append_revision(&published, &written.revision)
                .await
                .unwrap();
        }

        let page = store
            .sync(
                alice_id,
                &SyncQuery {
                    after: Some(head),
                    channels: vec![channel],
                    groups: Vec::new(),
                    page: PageRequest::first(),
                },
            )
            .await
            .unwrap();

        assert_eq!(page.events.len(), 1, "forty revisions become one entry");
        assert_eq!(
            page.compacted, 5,
            "and the client is told how many were folded"
        );

        let EventPayload::Revision(revision) = &page.events[0].payload else {
            panic!("the surviving event is the newest revision");
        };
        assert_eq!(revision.payload.title, "step 4");
    }

    #[tokio::test]
    async fn a_sync_cannot_be_widened_by_asking_for_a_channel_you_are_not_in() {
        let store = store!();
        let (alice_id, alice) = user();
        let (_, bob) = user();
        let mine = seeded_channel(&store, &alice).await;
        let theirs = seeded_channel(&store, &bob).await;
        let head = EventStore::head(&store).await.unwrap();

        publish(&store, &theirs, &bob, "not for alice").await;
        publish(&store, &mine, &alice, "for alice").await;

        let page = store
            .sync(
                alice_id,
                &SyncQuery {
                    after: Some(head),
                    channels: vec![mine.clone(), theirs],
                    groups: Vec::new(),
                    page: PageRequest::first(),
                },
            )
            .await
            .unwrap();

        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].audience.channel(), Some(&mine));
    }

    #[tokio::test]
    async fn the_head_does_not_move_backwards_when_the_log_is_pruned() {
        let store = store!();
        let (_, alice) = user();
        let channel = seeded_channel(&store, &alice).await;
        publish(&store, &channel, &alice, "old news").await;

        let before = EventStore::head(&store).await.unwrap();
        events(&store)
            .prune(now() + Duration::days(1), 1_000)
            .await
            .unwrap();
        let after = EventStore::head(&store).await.unwrap();

        assert!(
            after >= before,
            "a client told to start from now must not be handed a cursor that replays"
        );
    }

    // --- Idempotency, quotas, leadership -----------------------------------

    #[tokio::test]
    async fn critical_t16_a_claimed_key_replays_instead_of_publishing_twice() {
        let store = store!();
        let principal = format!("usr_{}", ulid_slug());
        let key = "deploy-1";

        assert_eq!(
            store.claim(&principal, key, now()).await.unwrap(),
            Claim::Fresh
        );
        assert_eq!(
            store.claim(&principal, key, now()).await.unwrap(),
            Claim::InFlight,
            "a retry arriving mid-flight waits its turn rather than publishing again"
        );

        store
            .complete(
                &principal,
                key,
                "{\"id\":\"ntf_1\"}",
                now() + Duration::hours(24),
            )
            .await
            .unwrap();

        assert_eq!(
            store.claim(&principal, key, now()).await.unwrap(),
            Claim::Replayed {
                response: "{\"id\":\"ntf_1\"}".to_owned()
            }
        );

        // Another publisher picking the same unimaginative key sees none of it.
        let other = format!("usr_{}", ulid_slug());
        assert_eq!(store.claim(&other, key, now()).await.unwrap(), Claim::Fresh);
    }

    #[tokio::test]
    async fn a_released_claim_frees_the_key_and_an_expired_one_lapses() {
        let store = store!();
        let principal = format!("usr_{}", ulid_slug());

        store.claim(&principal, "released", now()).await.unwrap();
        store.release(&principal, "released").await.unwrap();
        assert_eq!(
            store.claim(&principal, "released", now()).await.unwrap(),
            Claim::Fresh,
            "a client's retry must not be answered `409` for the rest of the window"
        );

        store.claim(&principal, "lapsed", now()).await.unwrap();
        assert_eq!(
            store
                .claim(&principal, "lapsed", now() + Duration::hours(1))
                .await
                .unwrap(),
            Claim::Fresh,
            "the replica holding it evidently died"
        );

        store.claim(&principal, "pruned", now()).await.unwrap();
        store
            .complete(&principal, "pruned", "{}", now() - Duration::hours(1))
            .await
            .unwrap();
        assert!(IdempotencyStore::prune(&store, now()).await.unwrap() >= 1);
    }

    // --- The audit log -----------------------------------------------------

    #[tokio::test]
    async fn every_action_this_build_emits_can_be_read_back() {
        // The write path takes any string and the read path only accepts a closed
        // list, so an action somebody emits without adding it to that list produces
        // a row that `GET /admin/audit` cannot hydrate — and the failure is not on
        // the row, it is on the *whole query*. One `PATCH /channels/{id}` would make
        // the audit log unreadable, which is the one log that has to survive an
        // incident (design §13).
        let store = store!();
        let (_, alice) = user();
        let channel = seeded_channel(&store, &alice).await;

        for action in notify_ports::AUDIT_ACTIONS {
            audit(&store)
                .record(&AuditEntry {
                    actor: alice.clone().into(),
                    action: *action,
                    target: Some("t".to_owned()),
                    channel: Some(channel.clone()),
                    detail: None,
                    at: now(),
                })
                .await
                .unwrap();
        }

        let page = audit(&store)
            .query(&AuditQuery {
                channel: Some(channel.clone()),
                page: PageRequest::of_size(100),
                ..AuditQuery::default()
            })
            .await
            .unwrap_or_else(|error| {
                panic!("an action this build writes cannot be read back: {error}")
            });

        for action in notify_ports::AUDIT_ACTIONS {
            assert!(
                page.items.iter().any(|entry| entry.action == *action),
                "{action} was written and did not come back"
            );
        }
    }

    #[tokio::test]
    async fn the_audit_log_is_browsable_by_actor_action_and_channel() {
        let store = store!();
        let (_, alice) = user();
        let (_, bob) = user();
        let channel = seeded_channel(&store, &alice).await;
        let at = now();

        for (actor, action) in [
            (&alice, notify_ports::actions::APIKEY_CREATED),
            (&alice, notify_ports::actions::APIKEY_REVOKED),
            (&bob, notify_ports::actions::DEVICE_ENROLLED),
        ] {
            audit(&store)
                .record(&AuditEntry {
                    actor: actor.clone().into(),
                    action,
                    target: Some("something".to_owned()),
                    channel: Some(channel.clone()),
                    detail: Some(serde_json::json!({ "scopes": ["publish"] })),
                    at,
                })
                .await
                .unwrap();
        }

        let by_actor = audit(&store)
            .query(&AuditQuery {
                actor: Some(alice.clone().into()),
                ..AuditQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(by_actor.items.len(), 2);
        assert_eq!(
            by_actor.items[0].action.as_str(),
            "apikey.revoked",
            "newest first: a review starts from what just happened"
        );

        // Narrowed by actor as well: the action filter is instance-wide by design,
        // and this suite deliberately leaves its rows behind, so a bare action
        // query would also see the ones an earlier run wrote.
        let by_action = audit(&store)
            .query(&AuditQuery {
                actor: Some(bob.clone().into()),
                action: Some("device.enrolled".to_owned()),
                ..AuditQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(by_action.items.len(), 1);
        assert_eq!(by_action.items[0].actor, bob.clone().into());
        assert_eq!(
            by_action.items[0].detail,
            Some(serde_json::json!({ "scopes": ["publish"] }))
        );

        let by_channel = audit(&store)
            .query(&AuditQuery {
                channel: Some(channel),
                ..AuditQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(by_channel.items.len(), 3);

        let out_of_range = audit(&store)
            .query(&AuditQuery {
                since: Some(at + Duration::hours(1)),
                ..AuditQuery::default()
            })
            .await
            .unwrap();
        assert!(out_of_range.is_empty());
    }

    #[tokio::test]
    async fn an_audit_page_follows_its_cursor_backwards() {
        let store = store!();
        let (_, alice) = user();

        for _ in 0..4 {
            audit(&store)
                .record(&AuditEntry {
                    actor: alice.clone().into(),
                    action: notify_ports::actions::CHANNEL_MEMBER_ADDED,
                    target: None,
                    channel: None,
                    detail: None,
                    at: now(),
                })
                .await
                .unwrap();
        }

        let first = audit(&store)
            .query(&AuditQuery {
                actor: Some(alice.clone().into()),
                page: PageRequest::of_size(2),
                ..AuditQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(first.items.len(), 2);
        let next = first.next.expect("there is more behind it");

        let second = audit(&store)
            .query(&AuditQuery {
                actor: Some(alice.into()),
                page: PageRequest::of_size(2).after(next),
                ..AuditQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(second.items.len(), 2);
        assert!(second.next.is_none());
    }

    #[tokio::test]
    async fn critical_t14_a_noisy_publisher_degrades_its_own_quota_and_nobody_elses() {
        let store = store!();
        let mine = Quota::TokenExchange {
            subject: format!("forgejo|{}", ulid_slug()),
        };
        let theirs = Quota::TokenExchange {
            subject: format!("forgejo|{}", ulid_slug()),
        };
        let at = now();

        // Ten a minute (design §8.2), consumed on the way in.
        for attempt in 1..=10 {
            let allowance = store.check(&mine, None, at).await.unwrap();
            assert!(allowance.is_allowed(), "attempt {attempt} should pass");
        }

        let refused = store.check(&mine, None, at).await.unwrap();
        let Allowance::Limited {
            retry_after_seconds,
        } = refused
        else {
            panic!("the eleventh is refused");
        };
        assert!((1..=60).contains(&retry_after_seconds));

        assert!(
            store.check(&theirs, None, at).await.unwrap().is_allowed(),
            "one publisher in a retry loop must not take the endpoint down for everybody"
        );

        // The next window starts clean.
        assert!(
            store
                .check(&mine, None, at + Duration::minutes(1))
                .await
                .unwrap()
                .is_allowed()
        );
        store
            .prune_rate_limits(now() + Duration::days(1))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn critical_t22_only_one_replica_leads_a_job_at_a_time() {
        let store = store!();
        let first = PgLeaderElection::new(store.pool().clone());
        let second = PgLeaderElection::new(store.pool().clone());

        let Some(guard) = first.try_acquire("purge").await.unwrap() else {
            eprintln!("another run holds the purge lock — skipping");
            return;
        };

        assert!(guard.is_held().await);
        assert!(
            second.try_acquire("purge").await.unwrap().is_none(),
            "two replicas purging concurrently is the failure this prevents"
        );

        // A different job is a different lock, so the two never serialise.
        let other = second.try_acquire("reconcile").await.unwrap();
        assert!(other.is_some());
        if let Some(other) = other {
            other.release().await;
        }

        guard.release().await;
        assert!(!guard.is_held().await, "released means released");

        let taken = second.try_acquire("purge").await.unwrap();
        assert!(
            taken.is_some(),
            "another replica picks it up without a timeout"
        );
        if let Some(taken) = taken {
            taken.release().await;
        }
    }

    #[tokio::test]
    async fn the_store_answers_readyz() {
        let store = store!();
        store.ping().await.expect("the database is reachable");
    }

    /// The whole point of `Arc<dyn Trait>`: the composition root wires one
    /// `PgStore` behind every port it implements.
    #[tokio::test]
    async fn one_store_satisfies_every_port_the_use_cases_ask_for() {
        let store = store!();
        let shared = Arc::new(store);

        let _: Arc<dyn NotificationRepository> = shared.clone();
        let _: Arc<dyn ChannelRepository> = shared.clone();
        let _: Arc<dyn UserRepository> = shared.clone();
        let _: Arc<dyn ServiceAccountRepository> = shared.clone();
        let _: Arc<dyn ApiKeyRepository> = shared.clone();
        let _: Arc<dyn DeviceRepository> = shared.clone();
        let _: Arc<dyn InteractionRepository> = shared.clone();
        let _: Arc<dyn ReceiptRepository> = shared.clone();
        let _: Arc<dyn EventStore> = shared.clone();
        let _: Arc<dyn IdempotencyStore> = shared.clone();
        let _: Arc<dyn ReplayGuard> = shared.clone();
        let _: Arc<dyn RateLimiter> = shared;
    }

    // --- Federation replay (design §5.3, scenario T11) --------------------

    #[tokio::test]
    async fn a_federated_token_id_is_remembered_exactly_once() {
        let store = store!();
        let jti = format!("tok-{}", ulid_slug());
        let until = OffsetDateTime::now_utc() + Duration::minutes(15);

        assert!(
            ReplayGuard::remember(&store, &jti, until).await.unwrap(),
            "the first exchange of an id has to be allowed"
        );
        assert!(
            !ReplayGuard::remember(&store, &jti, until).await.unwrap(),
            "a leaked ID token is worth one exchange, and the primary key is what \
             makes that true even for two requests arriving together"
        );
    }

    #[tokio::test]
    async fn the_purge_drops_ids_whose_token_has_expired_and_keeps_the_rest() {
        let store = store!();
        let now = OffsetDateTime::now_utc();
        let stale = format!("stale-{}", ulid_slug());
        let live = format!("live-{}", ulid_slug());

        ReplayGuard::remember(&store, &stale, now - Duration::minutes(1))
            .await
            .unwrap();
        ReplayGuard::remember(&store, &live, now + Duration::hours(1))
            .await
            .unwrap();

        ReplayGuard::prune(&store, now).await.unwrap();

        assert!(
            ReplayGuard::remember(&store, &stale, now + Duration::hours(1))
                .await
                .unwrap(),
            "past its token's expiry an id protects against nothing, so keeping it \
             is storage bought for no security"
        );
        assert!(
            !ReplayGuard::remember(&store, &live, now + Duration::hours(1))
                .await
                .unwrap(),
            "and one still inside its window must survive the sweep"
        );
    }

    // --- Outbound sinks (design §7.1bis, scenario T17) ---------------------

    fn queue(store: &PgStore) -> &dyn notify_ports::SinkQueue {
        store
    }

    /// One delivery of a real event, so the row round-trips what a consumer would
    /// actually receive rather than a fixture that only this file believes in.
    async fn delivery(store: &PgStore) -> notify_ports::SinkDelivery {
        let (id, alice) = user();
        let stored =
            User::from_claims(id, &format!("oidc-{}", ulid_slug()), None, None, now()).unwrap();
        users(store).upsert(&stored).await.unwrap();
        let channel = seeded_channel(store, &alice).await;
        let (notification, _) = publish(store, &channel, &alice, "etcd degraded").await;

        let page = events(store)
            .sync(
                id,
                &SyncQuery {
                    after: None,
                    channels: vec![channel.clone()],
                    groups: Vec::new(),
                    page: PageRequest::default(),
                },
            )
            .await
            .unwrap();

        let event = page
            .events
            .into_iter()
            .find(|event| event.payload.notification_id() == Some(notification.id))
            .expect("the publish appended its revision event");

        notify_ports::SinkDelivery {
            delivery_id: notify_domain::DeliveryId::generate(),
            channel: channel.clone(),
            sink: notify_domain::SinkId::new("webhook", "https://hooks.weebo.poc/incidents")
                .unwrap(),
            event,
            attempts: 0,
            first_failed_at: None,
        }
    }

    #[tokio::test]
    async fn a_claim_leases_rather_than_takes_and_an_expired_lease_owes_it_again() {
        let store = store!();
        let delivery = delivery(&store).await;
        let now = now();

        queue(&store).enqueue(&delivery, now).await.unwrap();

        let claimed = queue(&store)
            .claim_due(now, 10, now + Duration::seconds(60))
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(
            claimed[0].event, delivery.event,
            "what a consumer receives is what a client would have seen on its \
             stream, byte for byte (ADR-0013)"
        );

        assert!(
            queue(&store)
                .claim_due(now, 10, now + Duration::seconds(60))
                .await
                .unwrap()
                .iter()
                .all(|held| held.delivery_id != delivery.delivery_id),
            "two replicas both briefly leading must not POST the same webhook twice"
        );

        // The property the lease exists for: a drain killed mid-attempt leaves the
        // delivery owed rather than losing it.
        let later = now + Duration::seconds(61);
        assert!(
            queue(&store)
                .claim_due(later, 10, later + Duration::seconds(60))
                .await
                .unwrap()
                .iter()
                .any(|held| held.delivery_id == delivery.delivery_id)
        );

        queue(&store).complete(&delivery.delivery_id).await.unwrap();
    }

    #[tokio::test]
    async fn a_retry_reschedules_the_same_row_rather_than_adding_one() {
        let store = store!();
        let delivery = delivery(&store).await;
        let now = now();

        queue(&store).enqueue(&delivery, now).await.unwrap();
        queue(&store)
            .enqueue(
                &notify_ports::SinkDelivery {
                    attempts: 1,
                    first_failed_at: Some(now),
                    ..delivery.clone()
                },
                now + Duration::seconds(30),
            )
            .await
            .unwrap();

        let due = queue(&store)
            .claim_due(now + Duration::seconds(30), 10, now + Duration::seconds(90))
            .await
            .unwrap();
        let mine: Vec<_> = due
            .iter()
            .filter(|held| held.delivery_id == delivery.delivery_id)
            .collect();

        assert_eq!(mine.len(), 1, "one failure must not owe two deliveries");
        assert_eq!(mine[0].attempts, 1);
        // To the second: `timestamptz` keeps microseconds and `OffsetDateTime` keeps
        // nanoseconds, so an exact comparison here would be testing the precision of
        // the column rather than that the give-up clock survived the round trip.
        assert_eq!(
            mine[0].first_failed_at.map(OffsetDateTime::unix_timestamp),
            Some(now.unix_timestamp())
        );

        queue(&store).complete(&delivery.delivery_id).await.unwrap();
    }

    #[tokio::test]
    async fn critical_t17_a_buried_delivery_is_listed_counted_and_requeued() {
        let store = store!();
        let delivery = delivery(&store).await;
        let channel = delivery.channel.clone();
        let now = now();

        queue(&store).enqueue(&delivery, now).await.unwrap();
        queue(&store)
            .bury(
                &notify_ports::SinkDelivery {
                    attempts: 12,
                    first_failed_at: Some(now - Duration::hours(24)),
                    ..delivery.clone()
                },
                "connection refused",
                now,
            )
            .await
            .unwrap();

        // Buried means gone from the queue: owed and abandoned at once would have
        // the next drain sending what the console lists as given up on.
        assert!(
            queue(&store)
                .claim_due(now, 100, now + Duration::seconds(60))
                .await
                .unwrap()
                .iter()
                .all(|held| held.delivery_id != delivery.delivery_id)
        );

        let letters = queue(&store).dead_letters(Some(&channel)).await.unwrap();
        assert_eq!(letters.len(), 1);
        assert_eq!(letters[0].attempts, 12);
        assert_eq!(
            letters[0].sink.destination(),
            "https://hooks.weebo.poc/incidents"
        );
        assert_eq!(
            queue(&store)
                .dead_letter_depth()
                .await
                .unwrap()
                .into_iter()
                .find(|(id, _)| *id == channel),
            Some((channel.clone(), 1))
        );

        assert!(
            queue(&store)
                .requeue(&delivery.delivery_id, now)
                .await
                .unwrap()
        );
        assert!(
            !queue(&store)
                .requeue(&delivery.delivery_id, now)
                .await
                .unwrap(),
            "the second requeue has nothing to put back"
        );
        assert!(
            queue(&store)
                .dead_letters(Some(&channel))
                .await
                .unwrap()
                .is_empty()
        );

        let back = queue(&store)
            .claim_due(now, 100, now + Duration::seconds(60))
            .await
            .unwrap();
        let mine = back
            .iter()
            .find(|held| held.delivery_id == delivery.delivery_id)
            .expect("a requeue puts back something a drain can actually send");
        assert_eq!(mine.event, delivery.event);
        assert_eq!(
            mine.first_failed_at, None,
            "a requeue is a fresh chance, not a resumption of the day it spent \
             failing — otherwise the next failure buries it again"
        );

        queue(&store).complete(&delivery.delivery_id).await.unwrap();
    }
    // --- Migrations (T24, §3.4) ---------------------------------------------

    /// A database name nothing else in this run will pick.
    fn throwaway_database() -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        format!(
            "wnk_t24_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
    }

    #[tokio::test]
    async fn critical_t24_a_first_boot_builds_the_schema_and_a_second_replays_nothing() {
        // T24 word for word: "boot sur base vide → schéma complet ; re-boot →
        // idempotent, zéro migration rejouée". Both halves need a database nobody
        // has migrated yet, so this one creates its own and drops it after — the
        // shared test database is, by construction, already migrated.
        let Ok(url) =
            std::env::var("WNK_TEST_DATABASE_URL").or_else(|_| std::env::var("DATABASE_URL"))
        else {
            eprintln!("no WNK_TEST_DATABASE_URL or DATABASE_URL — skipping");
            return;
        };

        let admin = PgStore::connect(&url, 1).await.expect("the server answers");
        let name = throwaway_database();
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
            .execute(admin.pool())
            .await
            .expect("the test role may create a database");

        let fresh = url
            .rsplit_once('/')
            .map(|(head, _)| format!("{head}/{name}"))
            .unwrap();
        let outcome = migrate_twice(&fresh).await;

        // Dropped whatever happened above, so a failing assertion does not leave a
        // database behind on somebody's machine.
        let store_closed = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"
        )))
        .execute(admin.pool())
        .await;
        assert!(
            store_closed.is_ok(),
            "the throwaway database could not be dropped"
        );

        outcome.expect("see the assertion above");
    }

    /// The body of the test above, so its database is dropped even when it fails.
    async fn migrate_twice(url: &str) -> Result<(), String> {
        let store = PgStore::connect(url, 2).await.map_err(|e| e.to_string())?;

        store.migrate().await.map_err(|e| e.to_string())?;

        let tables: Vec<(String,)> = sqlx::query_as(
            "select table_name::text from information_schema.tables \
             where table_schema = 'public'",
        )
        .fetch_all(store.pool())
        .await
        .map_err(|e| e.to_string())?;
        let tables: Vec<String> = tables.into_iter().map(|(name,)| name).collect();

        for expected in [
            "channels",
            "notifications",
            "events",
            "devices",
            "sink_queue",
        ] {
            if !tables.iter().any(|name| name == expected) {
                return Err(format!(
                    "a boot on an empty database left no {expected} table: {tables:?}"
                ));
            }
        }

        //  is the field that betrays a replay: a migration applied a
        // second time would carry a new timestamp, even with the same version.
        let first: Vec<(i64, OffsetDateTime)> =
            sqlx::query_as("select version, installed_on from _sqlx_migrations order by version")
                .fetch_all(store.pool())
                .await
                .map_err(|e| e.to_string())?;

        if first.is_empty() {
            return Err("no migration was recorded at all".to_owned());
        }

        store.migrate().await.map_err(|e| e.to_string())?;

        let second: Vec<(i64, OffsetDateTime)> =
            sqlx::query_as("select version, installed_on from _sqlx_migrations order by version")
                .fetch_all(store.pool())
                .await
                .map_err(|e| e.to_string())?;

        if first != second {
            return Err(format!(
                "the second boot replayed something: {first:?} then {second:?}"
            ));
        }

        store.pool().close().await;
        Ok(())
    }
}
