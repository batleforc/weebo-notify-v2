//! Every query in this adapter against a real Postgres, matching the in-memory double's semantics
//! (the reason `sqlx::query!` is not needed). Under `mod e2e`; skipped without a database.
//! Tests share the database and isolate by fresh ULID-named channels.

#![expect(
    clippy::arithmetic_side_effects,
    reason = "test arithmetic on small, known values"
)]
#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a failing assertion is a failing test"
)]

mod e2e {
    use std::sync::Arc;

    use notify_domain::{
        ApiKey, ApiKeyId, ApiKeyOwner, ApiKeyPrefix, AttachmentId, Author, Channel, ChannelId,
        ChannelPolicy, CloseAfter, ContentDraft, Cursor, Day, Device, DeviceId, DevicePublicKey,
        Dnd, DndScope, ExternalId, GroupBinding, HumanDuration, Image, Interaction, InteractionLog,
        Membership, Notification, NotificationId, Principal, Priority, PushTransport, QuietHours,
        Receipt, ReceiptKind, RevisionDraft, Scope, ServiceAccount, ServiceAccountId, Silence,
        SilenceTarget, State, Status, TombstoneReason, Ttl, User, UserId, WallClock,
    };
    use notify_persistence_pg::{PgLeaderElection, PgStore};
    use notify_ports::{
        Allowance, ApiKeyRepository, AttachmentRegistry, AuditEntry, AuditLog, AuditQuery,
        ChannelRepository, Claim, Declaration, DeclarationKind, DeclarationStore, DeviceRepository,
        EmailMatch, EventPayload, EventStore, IdempotencyStore, InboxQuery, InteractionRepository,
        LeaderElection, NotificationRepository, PageRequest, PreferenceRepository, Quota,
        RateLimiter, ReceiptRepository, ReplayGuard, SearchQuery, ServiceAccountRepository,
        StoredApiKey, SyncQuery, UnreadCount, UserRepository, Visibility,
    };
    use time::{Duration, OffsetDateTime};

    fn database_url() -> Option<String> {
        std::env::var("WNK_TEST_DATABASE_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .ok()
    }

    /// The database under test, or `None` when there is not one.
    async fn store() -> Option<PgStore> {
        let url = database_url()?;

        let store = PgStore::connect(&url, 8)
            .await
            .expect("the configured database accepts connections");
        store.migrate().await.expect("migrations apply");

        Some(store)
    }

    /// No database configured: skip — unless `WNK_TEST_REQUIRE_DB` is set, which is
    /// how CI says "a database is supposed to be here". There a skip would report
    /// green for a suite that never ran, so it fails instead.
    fn skip_without_database() {
        assert!(
            std::env::var_os("WNK_TEST_REQUIRE_DB").is_none(),
            "WNK_TEST_REQUIRE_DB is set but neither WNK_TEST_DATABASE_URL nor DATABASE_URL is — refusing to skip"
        );
        eprintln!("no WNK_TEST_DATABASE_URL or DATABASE_URL — skipping");
    }

    /// Skip cleanly when no database is configured.
    macro_rules! store {
        () => {
            match store().await {
                Some(store) => store,
                None => {
                    skip_without_database();
                    return;
                }
            }
        };
    }

    // One `PgStore` implements eleven ports, so these name the port to disambiguate.
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

    /// A group name unique to this run, since rows are left behind.
    fn fresh_group() -> String {
        format!("g-{}", ulid_slug())
    }

    /// Eight unique characters: the tail of a ULID, since the head is the shared timestamp.
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
    async fn a_revision_written_from_a_stale_read_conflicts_instead_of_landing() {
        let store = store!();
        let (_, alice) = user();
        let channel = seeded_channel(&store, &alice).await;
        let (snapshot, _) = publish(&store, &channel, &alice, "deploy prod").await;
        let revise = |mut from: Notification| {
            let written = from
                .revise(
                    RevisionDraft::simple(ContentDraft::default()),
                    author(&alice),
                    now(),
                    &ChannelPolicy::default(),
                )
                .unwrap();
            (from, written.revision)
        };

        // Two writers read the same head and both computed seq 2.
        let (first, first_revision) = revise(snapshot.clone());
        let (second, second_revision) = revise(snapshot.clone());
        store
            .append_revision(&first, &first_revision)
            .await
            .unwrap();
        assert!(matches!(
            store.append_revision(&second, &second_revision).await,
            Err(notify_ports::PortError::Conflict { .. })
        ));

        // Deleted between the read and the write: no revision after the tombstone.
        let (current, _) = publish(&store, &channel, &alice, "nightly").await;
        let (revised, revision) = revise(current.clone());
        notifications(&store)
            .mark_deleted(current.id, now())
            .await
            .unwrap();
        assert!(matches!(
            store.append_revision(&revised, &revision).await,
            Err(notify_ports::PortError::Conflict { .. })
        ));
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

        // Losing the unique index must be recoverable (T16).
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
    async fn an_expired_holder_of_the_external_id_is_retired_even_while_the_sweep_has_it() {
        let store = store!();
        let (_, alice) = user();
        let channel = seeded_channel(&store, &alice).await;
        let external = ExternalId::parse("run-expired").unwrap();
        let (stale, _) = publish_with(
            &store,
            &channel,
            &alice,
            RevisionDraft {
                ttl: Some(Ttl::For(HumanDuration::parse("1h", "ttl").unwrap())),
                // Closed, so its retention clock runs.
                ..RevisionDraft::lifecycle(State::Closed, ContentDraft::titled("old run"))
            },
            Some(external.clone()),
            now() - Duration::days(30),
        )
        .await;

        // The sweep's lock, as `claim_expired` takes it, held across the publish.
        let mut sweep = store.pool().begin().await.unwrap();
        sqlx::query("SELECT 1 FROM notifications WHERE id = $1 FOR NO KEY UPDATE")
            .bind(stale.id.to_string())
            .execute(&mut *sweep)
            .await
            .unwrap();
        let (fresh, written) = Notification::create(
            NotificationId::generate(),
            channel.clone(),
            Some(external.clone()),
            RevisionDraft::lifecycle(State::Open, ContentDraft::titled("new run")),
            author(&alice),
            now(),
            &ChannelPolicy::default(),
        )
        .unwrap();
        let publishing = {
            let (store, fresh, revision) = (store.clone(), fresh.clone(), written.revision.clone());
            tokio::spawn(async move { notifications(&store).insert(&fresh, &revision).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            !publishing.is_finished(),
            "waits for the sweep, never steps over it"
        );
        sweep.rollback().await.unwrap();

        publishing
            .await
            .unwrap()
            .expect("the stale row no longer blocks the id");
        let found = store
            .find_by_external_id(&channel, &external)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, fresh.id);
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
        assert_eq!(page.expired, [] as [notify_ports::ExpiredWhileAway; 0]);
    }

    #[tokio::test]
    async fn the_expiry_sweep_skips_a_row_a_receipt_is_being_written_against() {
        let store = store!();
        let (_, alice) = user();
        let channel = seeded_channel(&store, &alice).await;
        let (published, _) = publish_with(
            &store,
            &channel,
            &alice,
            RevisionDraft {
                state: Some(State::Closed),
                ttl: Some(Ttl::For(HumanDuration::parse("1h", "ttl").unwrap())),
                ..RevisionDraft::simple(ContentDraft::titled("in flight"))
            },
            None,
            now() - Duration::days(30),
        )
        .await;

        // What `receipts::record` and `interactions` take first.
        let mut receipt = store.pool().begin().await.unwrap();
        sqlx::query("SELECT 1 FROM notifications WHERE id = $1 FOR SHARE")
            .bind(published.id.to_string())
            .execute(&mut *receipt)
            .await
            .unwrap();
        let claimed = store.claim_expired(now(), 1_000).await.unwrap();
        assert!(
            claimed.iter().all(|expired| expired.id != published.id),
            "claimed under a reader's lock: the receipt would count it unseen"
        );
        receipt.rollback().await.unwrap();

        // Released: the next sweep takes it. Others' rows may be ahead in the shared database.
        let mut taken = false;
        for _ in 0..10 {
            let claimed = store.claim_expired(now(), 1_000).await.unwrap();
            taken = claimed.iter().any(|expired| expired.id == published.id);
            if taken || claimed.is_empty() {
                break;
            }
        }
        assert!(taken);
    }

    #[tokio::test]
    async fn the_attachments_of_doomed_rows_are_named_and_the_others_are_not() {
        // `content->'image'->>'ref'` type-checks in Rust and can still match nothing in Postgres.
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
        // The registry records an id until a revision names it; `NOT EXISTS` decides.
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

        assert_eq!(registry(&store).forget(&[orphan]).await.unwrap(), 1);
        let stale = registry(&store)
            .unreferenced(now() - Duration::days(1), 100)
            .await
            .unwrap();
        assert!(!stale.contains(&orphan));
    }

    #[tokio::test]
    async fn an_attachment_is_visible_exactly_where_a_notification_shows_it() {
        // The authorisation behind `GET /attachments/{id}`: readable iff something showing it is.
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

        // Invisible once the last notification showing it is gone.
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
        // Guards the `NOT IN`-over-NULL trap.
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

    // --- Stale closure (§7.2bis) -------------------------------------------

    /// The whole stale-closure sweep: closed, retention starts, the purge sees it.
    #[tokio::test]
    async fn close_stale_closes_what_nobody_has_touched() {
        let store = store!();
        let (alice_id, alice) = user();
        let channel = seeded_channel(&store, &alice).await;

        let published_at = now() - Duration::days(9);
        let (published, _) = publish_with(
            &store,
            &channel,
            &alice,
            RevisionDraft::lifecycle(State::Open, ContentDraft::titled("deploy prod-eu-3")),
            None,
            published_at,
        )
        .await;
        assert_eq!(
            published.expires_at(),
            None,
            "an open notification has no deadline — that is the bug this closes"
        );

        let before = EventStore::head(&store).await.unwrap();
        let at = now();
        let closed = store
            .close_stale(&[(channel.clone(), at - Duration::days(7))], at, 100)
            .await
            .unwrap();

        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].id, published.id);
        assert_eq!(closed[0].channel, channel);

        let reloaded = NotificationRepository::get(&store, published.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            reloaded.head.state,
            State::Closed,
            "the read model has to agree with the sweep, or the inbox filters it \
             as closed while the detail view calls it open"
        );
        // Compared to the microsecond: `timestamptz` keeps six decimals.
        let closed_at = reloaded.closed_at.unwrap();
        assert!(
            (closed_at - at).abs() < Duration::milliseconds(1),
            "closed at {closed_at}, swept at {at}"
        );
        let expires_at = reloaded.expires_at().unwrap();
        assert!(
            (expires_at - (at + Duration::days(7))).abs() < Duration::milliseconds(1),
            "closing is what makes the channel's TTL apply at all; got {expires_at}"
        );

        // The event a connected client needs to mark its copy closed.
        let backlog = store
            .sync(
                alice_id,
                &SyncQuery {
                    after: Some(before),
                    channels: vec![channel.clone()],
                    groups: Vec::new(),
                    page: PageRequest::first(),
                },
            )
            .await
            .unwrap();
        assert!(
            backlog.events.iter().any(|event| matches!(
                &event.payload,
                EventPayload::Closure { notification_id, .. }
                    if *notification_id == published.id
            )),
            "a closure nobody is told about is a notification that stays open on \
             every screen already showing it"
        );
    }

    /// Both kinds of activity (revision and interaction) keep a notification open.
    #[tokio::test]
    async fn close_stale_leaves_anything_with_recent_activity_alone() {
        let store = store!();
        let (_, alice) = user();
        let channel = seeded_channel(&store, &alice).await;

        // Published long ago, revised an hour ago: still being attended to.
        let (revised, _) = publish_with(
            &store,
            &channel,
            &alice,
            RevisionDraft::lifecycle(State::Open, ContentDraft::titled("long incident")),
            None,
            now() - Duration::days(30),
        )
        .await;
        let mut aggregate = revised.clone();
        let written = aggregate
            .revise(
                RevisionDraft::lifecycle(State::Working, ContentDraft::titled("still on it")),
                Author::anonymous(alice.clone()),
                now() - Duration::hours(1),
                &ChannelPolicy::default(),
            )
            .unwrap();
        store
            .append_revision(&aggregate, &written.revision)
            .await
            .unwrap();

        // Acknowledged an hour ago: somebody is on it.
        let actions = notify_domain::Actions::new(vec![
            notify_domain::ActionDraft::Ack {
                key: "ack-incident".to_owned(),
                label: "Acknowledge".to_owned(),
            }
            .try_into()
            .unwrap(),
        ])
        .unwrap();
        let (acked, _) = publish_with(
            &store,
            &channel,
            &alice,
            RevisionDraft::lifecycle(
                State::Open,
                ContentDraft {
                    actions,
                    ..ContentDraft::titled("acked incident")
                },
            ),
            None,
            now() - Duration::days(30),
        )
        .await;
        let recorded = InteractionLog::default()
            .record(
                &acked.head.content.actions,
                acked.id,
                author(&alice),
                Interaction::Ack {
                    key: notify_domain::ActionKey::parse("ack-incident").unwrap(),
                },
                now() - Duration::hours(1),
            )
            .unwrap();
        InteractionRepository::apply(&store, &recorded)
            .await
            .unwrap();

        let at = now();
        let closed = store
            .close_stale(&[(channel.clone(), at - Duration::days(7))], at, 100)
            .await
            .unwrap();

        assert!(
            closed.is_empty(),
            "closed {closed:?} — a sweep that closes what somebody is working on \
             is worse than no sweep"
        );
    }

    /// A channel absent from the deadlines is never looked at.
    #[tokio::test]
    async fn close_stale_only_touches_the_channels_it_was_given() {
        let store = store!();
        let (_, alice) = user();
        let swept = seeded_channel(&store, &alice).await;
        let spared = seeded_channel(&store, &alice).await;

        let published_at = now() - Duration::days(30);
        let (doomed, _) = publish_with(
            &store,
            &swept,
            &alice,
            RevisionDraft::lifecycle(State::Open, ContentDraft::titled("swept")),
            None,
            published_at,
        )
        .await;
        let (untouched, _) = publish_with(
            &store,
            &spared,
            &alice,
            RevisionDraft::lifecycle(State::Open, ContentDraft::titled("opted out")),
            None,
            published_at,
        )
        .await;

        let at = now();
        let closed = store
            .close_stale(&[(swept.clone(), at - Duration::days(7))], at, 100)
            .await
            .unwrap();

        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].id, doomed.id);
        assert_eq!(
            NotificationRepository::get(&store, untouched.id)
                .await
                .unwrap()
                .unwrap()
                .head
                .state,
            State::Open
        );

        // And an empty list is not a query that matches everything.
        assert_eq!(
            store.close_stale(&[], at, 100).await.unwrap(),
            [] as [notify_ports::ClosedNotification; 0]
        );
    }

    /// Idempotent: a closed notification is not a candidate again.
    #[tokio::test]
    async fn close_stale_is_idempotent() {
        let store = store!();
        let (_, alice) = user();
        let channel = seeded_channel(&store, &alice).await;

        publish_with(
            &store,
            &channel,
            &alice,
            RevisionDraft::lifecycle(State::Open, ContentDraft::titled("abandoned")),
            None,
            now() - Duration::days(30),
        )
        .await;

        let at = now();
        let deadlines = [(channel.clone(), at - Duration::days(7))];
        assert_eq!(
            store.close_stale(&deadlines, at, 100).await.unwrap().len(),
            1
        );
        assert_eq!(
            store
                .close_stale(&deadlines, at + Duration::hours(1), 100)
                .await
                .unwrap(),
            [] as [notify_ports::ClosedNotification; 0]
        );
    }

    /// A simple notification is born closed, so never a candidate.
    #[tokio::test]
    async fn close_stale_ignores_simple_notifications() {
        let store = store!();
        let (_, alice) = user();
        let channel = seeded_channel(&store, &alice).await;

        let (simple, _) = publish_with(
            &store,
            &channel,
            &alice,
            RevisionDraft::simple(ContentDraft::titled("brief")),
            None,
            now() - Duration::days(30),
        )
        .await;
        let deadline = simple.expires_at();

        let at = now();
        assert_eq!(
            store
                .close_stale(&[(channel.clone(), at - Duration::days(7))], at, 100)
                .await
                .unwrap(),
            [] as [notify_ports::ClosedNotification; 0]
        );
        let after = NotificationRepository::get(&store, simple.id)
            .await
            .unwrap()
            .unwrap()
            .expires_at()
            .unwrap();
        assert!(
            (after - deadline.unwrap()).abs() < Duration::milliseconds(1),
            "its clock started when it was published and must not be restarted"
        );
    }

    /// `all_policies` must round-trip `close_after` through JSON.
    #[tokio::test]
    async fn all_policies_round_trips_close_after() {
        let store = store!();
        let (_, alice) = user();
        let id = seeded_channel(&store, &alice).await;

        let mut channel = channels(&store).get(&id).await.unwrap().unwrap();
        channel.policy.close_after = CloseAfter::Never;
        channels(&store).upsert(&channel).await.unwrap();

        let policies = channels(&store).all_policies().await.unwrap();
        let stored = policies
            .into_iter()
            .find(|(channel, _)| *channel == id)
            .expect("the channel that was just written");
        assert_eq!(stored.1.close_after, CloseAfter::Never);
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
    async fn creating_a_name_that_exists_conflicts_instead_of_taking_it_over() {
        let store = store!();
        let (_, alice) = user();
        let (_, mallory) = user();
        let id = seeded_channel(&store, &alice).await;

        let mut rival = Channel::dynamic(id.clone(), None).unwrap();
        rival.upsert_member(Membership::new(mallory.clone(), vec![Scope::Manage]).unwrap());

        assert!(matches!(
            channels(&store).insert(&rival).await,
            Err(notify_ports::PortError::Conflict { .. })
        ));
        let found = ChannelRepository::get(&store, &id).await.unwrap().unwrap();
        assert!(found.grants(&alice, Scope::Subscribe));
        assert!(!found.grants(&mallory, Scope::Manage));
    }

    #[tokio::test]
    async fn member_and_policy_writes_leave_everything_else_alone() {
        let store = store!();
        let (_, alice) = user();
        let (_, bob) = user();
        let (_, carol) = user();
        let id = seeded_channel(&store, &alice).await;

        // Two managers acting on the same snapshot: neither write may undo the other.
        let snapshot = ChannelRepository::get(&store, &id).await.unwrap().unwrap();
        channels(&store)
            .put_member(
                &id,
                &Membership::new(bob.clone(), vec![Scope::Subscribe]).unwrap(),
            )
            .await
            .unwrap();
        channels(&store)
            .put_member(
                &id,
                &Membership::new(carol.clone(), vec![Scope::Subscribe]).unwrap(),
            )
            .await
            .unwrap();
        let mut policy = snapshot.policy;
        policy.receipts_enabled = !policy.receipts_enabled;
        channels(&store).set_policy(&id, &policy).await.unwrap();

        let found = ChannelRepository::get(&store, &id).await.unwrap().unwrap();
        assert!(found.grants(&bob, Scope::Subscribe));
        assert!(found.grants(&carol, Scope::Subscribe));
        assert_eq!(found.policy, policy);

        assert!(channels(&store).remove_member(&id, &bob).await.unwrap());
        assert!(
            !channels(&store).remove_member(&id, &bob).await.unwrap(),
            "the second removal finds nobody, so only one is journalled"
        );
        let found = ChannelRepository::get(&store, &id).await.unwrap().unwrap();
        assert!(!found.grants(&bob, Scope::Subscribe));
        assert!(found.grants(&carol, Scope::Subscribe));
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

        // The backlog follows the group door too.
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
        // A claimed field and a timezone, covering the profile columns (§2.4).
        user.edit_profile(notify_domain::ProfileEdit {
            display_name: Some(notify_domain::FieldEdit::Set("alice".to_owned())),
            timezone: Some(notify_domain::FieldEdit::Set("Europe/Paris".to_owned())),
            ..notify_domain::ProfileEdit::default()
        })
        .unwrap();

        users(&store).upsert(&user).await.unwrap();

        let stored = UserRepository::get(&store, user.id).await.unwrap().unwrap();
        assert_eq!(stored.id, user.id);
        // The override set must survive whole (§2.4).
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
        // Case-insensitive, so changed capitalisation does not orphan a binding.
        match store.find_by_email(&email.to_uppercase()).await.unwrap() {
            EmailMatch::One(found) => assert_eq!(found.id, user.id),
            other => panic!("expected one user, got {other:?}"),
        }
        assert!(store.find_by_subject("nobody").await.unwrap().is_none());

        // `users.email` is not unique. A second holder of the address — any case —
        // makes it name nobody, rather than whichever row the planner liked.
        let twin = User::from_claims(
            UserId::generate(),
            &format!("oidc-{}", ulid_slug()),
            Some(&email.to_uppercase()),
            None,
            now(),
        )
        .unwrap();
        users(&store).upsert(&twin).await.unwrap();
        assert_eq!(
            store.find_by_email(&email).await.unwrap(),
            EmailMatch::Ambiguous
        );

        // `/avatars/{id}` serves only what some user references as an avatar.
        let avatar = notify_domain::AttachmentId::generate();
        assert!(!store.is_avatar(avatar).await.unwrap());
        assert_eq!(
            users(&store).set_avatar(stored.id, avatar).await.unwrap(),
            None
        );
        assert!(store.is_avatar(avatar).await.unwrap());

        // The avatar has one writer: a stale profile written back does not undo it, and
        // the next upload is told exactly what it replaced.
        users(&store).upsert(&stored).await.unwrap();
        assert!(
            store.is_avatar(avatar).await.unwrap(),
            "a stale upsert put the old avatar back"
        );
        let next = notify_domain::AttachmentId::generate();
        assert_eq!(
            users(&store).set_avatar(stored.id, next).await.unwrap(),
            Some(avatar)
        );

        // The receipts roll names its readers in one read; unknown ids are left out.
        let named = users(&store)
            .get_many(&[stored.id, UserId::generate()])
            .await
            .unwrap();
        assert_eq!(named.len(), 1);
        assert_eq!(named[0].id, stored.id);
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
    async fn a_channels_keys_are_listed_by_that_channel() {
        let store = store!();
        let channel = fresh_channel();
        let key = ApiKey {
            id: ApiKeyId::generate(),
            prefix: ApiKeyPrefix::parse(fresh_prefix().as_str()).unwrap(),
            label: None,
            owner: ApiKeyOwner::Channel {
                id: channel.clone(),
            },
            scopes: vec![Scope::Publish],
            channels: vec![channel.clone()],
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

        let listed = keys(&store).list_for_channel(&channel).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, key.id);
        assert_eq!(
            keys(&store)
                .list_for_channel(&fresh_channel())
                .await
                .unwrap(),
            [] as [notify_domain::ApiKey; 0]
        );
    }

    #[tokio::test]
    async fn a_platform_admins_ceiling_on_a_key_round_trips_through_a_widened_column() {
        // The `BIGINT` column holds values near the top of `u32`.
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
    async fn enrolments_racing_at_the_device_cap_land_exactly_one() {
        let store = store!();
        let (_, alice) = user();
        let enrol = |label: &str| Device {
            id: DeviceId::generate(),
            owner: alice.clone(),
            label: label.to_owned(),
            transport: PushTransport::Stream,
            public_key: None,
            cursor: None,
            enrolled_at: now(),
            last_seen_at: None,
            revoked_at: None,
        };

        // One short of a cap of three, and a revoked one that must not count.
        let revoked = enrol("old");
        devices(&store).upsert(&revoked).await.unwrap();
        devices(&store).revoke(revoked.id, now()).await.unwrap();
        for label in ["laptop", "phone"] {
            assert!(
                devices(&store)
                    .enrol_capped(&enrol(label), 3)
                    .await
                    .unwrap()
            );
        }

        let racers: Vec<_> = (0..8)
            .map(|n| {
                let store = store.clone();
                let device = enrol(&format!("racer {n}"));
                tokio::spawn(async move { store.enrol_capped(&device, 3).await.unwrap() })
            })
            .collect();
        let mut landed = 0;
        for racer in racers {
            landed += usize::from(racer.await.unwrap());
        }

        assert_eq!(landed, 1, "count and insert are one step per owner");
        let active = DeviceRepository::list_for(&store, &alice)
            .await
            .unwrap()
            .iter()
            .filter(|device| device.is_active())
            .count();
        assert_eq!(active, 3);
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
        // Two tabs: the older ack must not win.
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

        // A later snapshot without the group means access is gone (§11).
        devices(&store)
            .remember_groups(&alice, &[fresh_group()], now())
            .await
            .unwrap();
        assert_eq!(
            store.delivery_targets(&id).await.unwrap(),
            [] as [notify_domain::Device; 0]
        );

        // What a personal key reads back: the snapshot, only while it is fresh.
        let taken = now();
        assert_eq!(
            devices(&store)
                .remembered_groups(&alice, taken - time::Duration::days(7))
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            devices(&store)
                .remembered_groups(&alice, taken + time::Duration::days(1))
                .await
                .unwrap(),
            [] as [std::string::String; 0]
        );
    }

    // --- What GitOps last applied (design §11, §13, D28) ---------------------

    #[tokio::test]
    async fn a_declaration_set_is_replaced_and_not_merged() {
        let store = store!();

        // Namespaced per run. Invasive: `apply` replaces a whole kind, so against a shared database
        // the reconciler's next pass will audit spurious `added`/`removed` entries.
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

        // A withdrawn trust policy must stop being stored.
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

        // The value must read back, or `GET /admin/audit` breaks.
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

    #[tokio::test]
    async fn critical_old_audit_webhook_urls_are_redacted_and_the_log_stays_append_only() {
        // ADR-0026: the one sanctioned edit to the audit log. An entry journalled
        // before sink ids were redacted still carries the webhook url — and the
        // credential in its path. Planted here in that old shape, then the
        // migration is run again (it is idempotent by design) to show it rewrites
        // exactly that, to the origin and a keyless marker, and leaves the
        // append-only trigger armed behind it.
        const MIGRATION: &str =
            include_str!("../migrations/20261010300000_redact_audit_webhook_urls.sql");

        let store = store!();
        let channel = seeded_channel(&store, &user().1).await;
        let secret = "https://hooks.slack.com/services/T0/B0/SECRETPATH";
        let kept = "https://hooks.example#0123456789abcdef";

        audit(&store)
            .record(&AuditEntry {
                actor: notify_ports::AuditActor::configuration(),
                action: notify_ports::actions::MANAGED_CHANNEL_CHANGED,
                target: Some(channel.as_str().to_owned()),
                channel: Some(channel.clone()),
                detail: Some(serde_json::json!({
                    "declaration": { "sinks": [
                        { "type": "webhook", "url": secret },
                        { "type": "email", "to": "ops@example.org" },
                    ] },
                    "previous": { "sinks": [{ "type": "webhook", "url": kept }] },
                })),
                at: now(),
            })
            .await
            .unwrap();

        // Its own connection: the migration's `pg_temp` helpers die with it. The
        // purge migration (ADR-0027) runs again behind it, as it did on the way
        // up: the redaction's `CREATE OR REPLACE` restores the refuse-everything
        // trigger function, and leaving it there would close the purge's door for
        // every test after this one.
        let mut connection = store.pool().acquire().await.unwrap().detach();
        let mut tx = sqlx::Connection::begin(&mut connection).await.unwrap();
        sqlx::raw_sql(MIGRATION).execute(&mut *tx).await.unwrap();
        sqlx::raw_sql(AUDIT_PURGE_MIGRATION)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let back = audit(&store)
            .query(&AuditQuery {
                channel: Some(channel.clone()),
                ..AuditQuery::default()
            })
            .await
            .unwrap();
        assert_eq!(back.items.len(), 1);
        let detail = back.items[0].detail.as_ref().unwrap();
        assert_eq!(
            detail["declaration"]["sinks"][0]["url"], "https://hooks.slack.com#redacted",
            "the origin survives; nothing that could be brute-forced back to the path does"
        );
        assert_eq!(
            detail["declaration"]["sinks"][1],
            serde_json::json!({ "type": "email", "to": "ops@example.org" })
        );
        assert_eq!(
            detail["previous"]["sinks"][0]["url"], kept,
            "an already-redacted url is left alone"
        );
        assert!(!detail.to_string().contains("SECRETPATH"));

        // The redaction is in the log. Not asserted to be *this* run's entry — a
        // concurrent run may have redacted the row first — only that one exists.
        // Other tests write every action with no detail, hence the `and_then`.
        let noted = audit(&store)
            .query(&AuditQuery {
                action: Some("audit.redacted".to_owned()),
                ..AuditQuery::default()
            })
            .await
            .unwrap();
        assert!(
            noted
                .items
                .iter()
                .filter_map(|entry| entry.detail.as_ref())
                .any(|detail| detail["redacted_entries"].as_u64() >= Some(1)),
            "the redaction recorded itself"
        );

        // And the guard is back. Each attempt in a transaction that is rolled
        // back, so a regression here cannot wipe the shared database's log.
        for statement in [
            "UPDATE audit_entries SET detail = NULL WHERE channel_id = $1",
            "DELETE FROM audit_entries WHERE channel_id = $1",
            "TRUNCATE audit_entries",
        ] {
            let mut tx = store.pool().begin().await.unwrap();
            let query = if statement.contains("$1") {
                sqlx::query(statement).bind(channel.as_str())
            } else {
                sqlx::query(statement)
            };
            let refused = query.execute(&mut *tx).await.unwrap_err();
            assert!(
                refused.to_string().contains("append-only"),
                "`{statement}` must be refused, got: {refused}"
            );
            tx.rollback().await.unwrap();
        }
    }

    const AUDIT_PURGE_MIGRATION: &str =
        include_str!("../migrations/20261010800000_audit_purge.sql");

    /// One audit entry in `channel`, recorded `age` ago.
    async fn audit_entry_aged(store: &PgStore, channel: &ChannelId, age: Duration) {
        audit(store)
            .record(&AuditEntry {
                actor: notify_ports::AuditActor::configuration(),
                action: notify_ports::actions::TRUST_POLICY_ADDED,
                target: Some(format!("{}d", age.whole_days())),
                channel: Some(channel.clone()),
                detail: None,
                at: now() - age,
            })
            .await
            .unwrap();
    }

    /// The ages, in days, of what `channel` still holds in the log, oldest first.
    async fn audit_ages(store: &PgStore, channel: &ChannelId) -> Vec<String> {
        let mut ages: Vec<String> = audit(store)
            .query(&AuditQuery {
                channel: Some(channel.clone()),
                ..AuditQuery::default()
            })
            .await
            .unwrap()
            .items
            .into_iter()
            .filter_map(|entry| entry.target)
            .collect();
        ages.reverse();
        ages
    }

    #[tokio::test]
    async fn critical_the_audit_purge_removes_only_old_rows_and_records_itself() {
        // ADR-0027: the one sanctioned deletion. Past the cutoff goes, inside it
        // stays — including a row the floor would allow but the cutoff does not.
        let store = store!();
        let channel = seeded_channel(&store, &user().1).await;
        for days in [500, 400, 100, 1] {
            audit_entry_aged(&store, &channel, Duration::days(days)).await;
        }

        // Batches of one, so the loop is what drains it — the shape the leader runs.
        let cutoff = now() - Duration::days(365);
        let mut purged = 0;
        loop {
            let batch = audit(&store).purge_older_than(cutoff, 1).await.unwrap();
            purged += batch;
            if batch < 1 {
                break;
            }
        }
        assert!(
            purged >= 2,
            "both of this test's old rows went, got {purged}"
        );
        assert_eq!(audit_ages(&store, &channel).await, ["100d", "1d"]);

        let noted = audit(&store)
            .query(&AuditQuery {
                action: Some("audit.purged".to_owned()),
                since: Some(now() - Duration::minutes(5)),
                ..AuditQuery::default()
            })
            .await
            .unwrap();
        assert!(
            noted
                .items
                .iter()
                .filter_map(|entry| entry.detail.as_ref())
                .any(|detail| detail["purged_entries"].as_u64() >= Some(1)
                    && detail["adr"] == "0027"),
            "every batch that deleted something says so in the log it deleted from"
        );
    }

    #[tokio::test]
    async fn critical_the_audit_purge_is_the_only_door_and_it_has_a_floor() {
        let store = store!();
        let channel = seeded_channel(&store, &user().1).await;
        audit_entry_aged(&store, &channel, Duration::days(400)).await;
        audit_entry_aged(&store, &channel, Duration::days(10)).await;

        // A cutoff inside 90 days is refused by the database, whatever the caller
        // was configured with — the floor an attacker with these credentials meets.
        let refused = audit(&store)
            .purge_older_than(now() - Duration::days(30), 100)
            .await
            .unwrap_err();
        assert!(
            format!("{refused:?}").contains("90-day floor"),
            "{refused:?}"
        );
        assert_eq!(audit_ages(&store, &channel).await, ["400d", "10d"]);

        // Every attempt in a transaction that is rolled back, so a regression
        // cannot wipe the shared database's log.
        let far_future = "SET LOCAL wnk.audit_purge = '2999-01-01T00:00:00Z'";
        for (setup, statement) in [
            // No setting: the old rule, unchanged.
            (None, "DELETE FROM audit_entries WHERE channel_id = $1"),
            // The setting forged by hand still cannot reach inside the floor.
            (
                Some(far_future),
                "DELETE FROM audit_entries WHERE channel_id = $1 AND target = '10d'",
            ),
            // The door closes when the function returns, even in its transaction.
            (
                Some("SELECT wnk_audit_purge(now() - interval '1000 days', 1)"),
                "DELETE FROM audit_entries WHERE channel_id = $1 AND target = '400d'",
            ),
            // UPDATE and TRUNCATE never go through, setting or not.
            (
                Some(far_future),
                "UPDATE audit_entries SET detail = NULL WHERE channel_id = $1",
            ),
            (Some(far_future), "TRUNCATE audit_entries"),
        ] {
            let mut tx = store.pool().begin().await.unwrap();
            if let Some(setup) = setup {
                sqlx::query(setup).execute(&mut *tx).await.unwrap();
            }
            let query = if statement.contains("$1") {
                sqlx::query(statement).bind(channel.as_str())
            } else {
                sqlx::query(statement)
            };
            let refused = query.execute(&mut *tx).await.unwrap_err();
            assert!(
                refused.to_string().contains("append-only"),
                "`{statement}` after {setup:?} must be refused, got: {refused}"
            );
            tx.rollback().await.unwrap();
        }
        assert_eq!(audit_ages(&store, &channel).await, ["400d", "10d"]);
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
        // One aggregate per channel; a read channel is absent, not zero.
        assert_eq!(
            notifications(&store)
                .unread_counts(&alice, std::slice::from_ref(&channel))
                .await
                .unwrap(),
            [] as [notify_ports::UnreadCount; 0]
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

        // Sharing a channel must not expose alice's silences to bob.
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

        // Re-silencing extends rather than stacks.
        store.silence(&silence).await.unwrap();
        assert_eq!(
            store.active_silences(alice_id, now()).await.unwrap().len(),
            1
        );

        let lapsed = store
            .active_silences(alice_id, now() + Duration::hours(3))
            .await
            .unwrap();
        assert_eq!(lapsed, [] as [notify_domain::Silence; 0]);
    }

    // --- Do not disturb (design §7.4) --------------------------------------

    #[tokio::test]
    async fn a_preference_nobody_set_reads_as_one_that_mutes_nothing() {
        let store = store!();
        let (alice_id, _) = user();

        // No row is not an error: same as a mute-nothing preference.
        let dnd = PreferenceRepository::dnd(&store, alice_id).await.unwrap();

        assert_eq!(dnd.user, alice_id);
        assert_eq!(dnd.quiet_hours, [] as [notify_domain::QuietHours; 0]);
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

        let held = NotificationId::generate();
        for (seq, status) in [
            (1, Status::Info),
            (2, Status::Failure),
            (3, Status::Success),
        ] {
            PreferenceRepository::defer(&store, alice_id, held, seq, status)
                .await
                .unwrap();
        }
        // The same revisions again — a retried outbox row, a second device: the
        // digest's number must not move.
        for (seq, status) in [(1, Status::Info), (2, Status::Failure)] {
            PreferenceRepository::defer(&store, alice_id, held, seq, status)
                .await
                .unwrap();
        }

        let mine = store
            .deferred_tallies(None, u32::MAX)
            .await
            .unwrap()
            .into_iter()
            .find(|tally| tally.dnd.user == alice_id)
            .expect("a tally with something on it comes back");

        assert_eq!(mine.total, 3);
        assert_eq!(mine.failures, 1, "the one status a digest calls out");

        PreferenceRepository::clear_deferred(&store, alice_id, 3, 1)
            .await
            .unwrap();
        assert!(
            !store
                .deferred_tallies(None, u32::MAX)
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

        // No row: matching nothing is correct.
        PreferenceRepository::defer(
            &store,
            alice_id,
            NotificationId::generate(),
            1,
            Status::Failure,
        )
        .await
        .unwrap();

        assert!(
            !store
                .deferred_tallies(None, u32::MAX)
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
        PreferenceRepository::defer(
            &store,
            alice_id,
            NotificationId::generate(),
            1,
            Status::Failure,
        )
        .await
        .unwrap();

        // A redefined quiet period invalidates the pending digest.
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
                .deferred_tallies(None, u32::MAX)
                .await
                .unwrap()
                .iter()
                .any(|tally| tally.dnd.user == alice_id)
        );
    }

    /// A `silence` must not ship a `preference` event missing the do-not-disturb half.
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
        assert_eq!(nothing.events, [] as [notify_ports::Event; 0]);
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
    async fn paging_one_event_at_a_time_reads_the_same_compacted_stream() {
        // The page is merged from one bounded scan per channel; walking it one row
        // at a time must yield exactly what one big page does, in `seq` order, with
        // the same compaction across channels that interleave.
        let store = store!();
        let (alice_id, alice) = user();
        let first = seeded_channel(&store, &alice).await;
        let second = seeded_channel(&store, &alice).await;
        let head = EventStore::head(&store).await.unwrap();

        let (mut a, _) = publish(&store, &first, &alice, "a").await;
        publish(&store, &second, &alice, "b").await;
        for step in 0..2 {
            let written = a
                .revise(
                    RevisionDraft::simple(ContentDraft::titled(&format!("a{step}"))),
                    author(&alice),
                    now(),
                    &ChannelPolicy::default(),
                )
                .unwrap();
            store.append_revision(&a, &written.revision).await.unwrap();
        }
        publish(&store, &second, &alice, "c").await;

        let query = |after: Cursor, limit: u32| SyncQuery {
            after: Some(after),
            channels: vec![first.clone(), second.clone()],
            groups: Vec::new(),
            page: PageRequest::of_size(limit),
        };
        let whole = store.sync(alice_id, &query(head, 50)).await.unwrap();
        let whole: Vec<Cursor> = whole.events.iter().map(|event| event.cursor).collect();

        let mut walked = Vec::new();
        let mut cursor = head;
        loop {
            let page = store.sync(alice_id, &query(cursor, 1)).await.unwrap();
            walked.extend(page.events.iter().map(|event| event.cursor));
            match page.next {
                Some(next) => cursor = next,
                None => break,
            }
        }

        assert_eq!(whole.len(), 3, "b, a's last revision, c");
        assert_eq!(walked, whole);
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
        let claimed_at = now();

        assert_eq!(
            store.claim(&principal, key, claimed_at).await.unwrap(),
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
                claimed_at,
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

        let claimed_at = now();
        store
            .claim(&principal, "released", claimed_at)
            .await
            .unwrap();
        store
            .release(&principal, "released", claimed_at)
            .await
            .unwrap();
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

        let claimed_at = now();
        store.claim(&principal, "pruned", claimed_at).await.unwrap();
        store
            .complete(
                &principal,
                "pruned",
                claimed_at,
                "{}",
                now() - Duration::hours(1),
            )
            .await
            .unwrap();
        assert!(IdempotencyStore::prune(&store, now()).await.unwrap() >= 1);
    }

    #[tokio::test]
    async fn a_claim_taken_over_cannot_be_completed_or_released_by_its_first_owner() {
        let store = store!();
        let principal = format!("usr_{}", ulid_slug());
        let first = now();
        let second = first + Duration::hours(1);
        let window = second + Duration::hours(24);

        store.claim(&principal, "slow", first).await.unwrap();
        // The first request stalled past its in-flight window; a retry took over.
        assert_eq!(
            store.claim(&principal, "slow", second).await.unwrap(),
            Claim::Fresh
        );

        store
            .complete(&principal, "slow", first, "{\"from\":\"first\"}", window)
            .await
            .unwrap();
        store.release(&principal, "slow", first).await.unwrap();
        assert_eq!(
            store.claim(&principal, "slow", second).await.unwrap(),
            Claim::InFlight,
            "the stale owner must neither overwrite nor free the successor's claim"
        );

        store
            .complete(&principal, "slow", second, "{\"from\":\"second\"}", window)
            .await
            .unwrap();
        assert_eq!(
            store.claim(&principal, "slow", second).await.unwrap(),
            Claim::Replayed {
                response: "{\"from\":\"second\"}".to_owned()
            }
        );
    }

    // --- The audit log -----------------------------------------------------

    #[tokio::test]
    async fn every_action_this_build_emits_can_be_read_back() {
        // An action outside the closed vocabulary would make the whole audit query fail (§13).
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

        // Narrowed by actor, since earlier runs' rows remain.
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
    async fn users_are_found_by_name_or_email_with_wildcards_matched_literally() {
        let store = store!();
        let (id, _) = user();
        let tag = id.to_string().to_lowercase();
        let person = User::from_claims(
            id,
            &format!("sub-{tag}"),
            Some(&format!("{tag}@weebo.poc")),
            Some("Zoë 100% Uptime"),
            now(),
        )
        .unwrap();
        UserRepository::upsert(&store, &person).await.unwrap();

        let by_mail = UserRepository::lookup(&store, &tag.to_uppercase(), 10)
            .await
            .unwrap();
        assert_eq!(by_mail.len(), 1);
        let by_name = UserRepository::lookup(&store, "100%", 10).await.unwrap();
        assert!(by_name.iter().any(|u| u.id == id));
        let literal = UserRepository::lookup(&store, "1_0%", 10).await.unwrap();
        assert!(!literal.iter().any(|u| u.id == id), "`_` is not a wildcard");
    }

    #[tokio::test]
    async fn stream_leases_cap_a_principal_across_replicas_and_expire_unrenewed() {
        use notify_ports::{Acquired, LeaseRequest, StreamLeases};
        let store = store!();
        let me = format!("user:{}", UserId::generate());
        let you = format!("user:{}", UserId::generate());
        let ask = |principal: &str, replica: &str, token: &str| LeaseRequest {
            principal: principal.to_owned(),
            replica: replica.to_owned(),
            transport: "sse".to_owned(),
            client: "Firefox".to_owned(),
            credential: "session".to_owned(),
            fingerprint: format!("{principal}-{token}"),
        };
        let granted = |acquired: Acquired| match acquired {
            Acquired::Granted(id) => id,
            other => panic!("expected a lease, got {other:?}"),
        };

        // Two replicas share one count.
        let a = granted(
            StreamLeases::acquire(&store, &ask(&me, "replica-a", "t1"), 2)
                .await
                .unwrap(),
        );
        granted(
            StreamLeases::acquire(&store, &ask(&me, "replica-b", "t2"), 2)
                .await
                .unwrap(),
        );
        assert_eq!(
            StreamLeases::acquire(&store, &ask(&me, "replica-a", "t3"), 2)
                .await
                .unwrap(),
            Acquired::Full
        );
        granted(
            StreamLeases::acquire(&store, &ask(&you, "replica-a", "t1"), 2)
                .await
                .unwrap(),
        );

        // Its owner sees both, with what each said about itself.
        let mine = StreamLeases::list(&store, &me).await.unwrap();
        assert_eq!(mine.len(), 2);
        assert_eq!(mine[0].client, "Firefox");

        // Disconnecting frees the slot, keeps that credential out, and is owner-only.
        assert!(
            !StreamLeases::disconnect(&store, &you, &a, now() + Duration::hours(1))
                .await
                .unwrap()
        );
        assert!(
            StreamLeases::disconnect(&store, &me, &a, now() + Duration::hours(1))
                .await
                .unwrap()
        );
        assert_eq!(
            StreamLeases::acquire(&store, &ask(&me, "replica-a", "t1"), 2)
                .await
                .unwrap(),
            Acquired::Blocked
        );
        granted(
            StreamLeases::acquire(&store, &ask(&me, "replica-a", "t4"), 2)
                .await
                .unwrap(),
        );
        let live = StreamLeases::renew(&store, "replica-a", core::slice::from_ref(&a))
            .await
            .unwrap();
        assert!(!live.contains(&a), "the replica learns its lease is gone");

        // A lease the replica no longer holds open (its release failed) is not renewed:
        // it lapses instead of counting against its owner for as long as the pod lives.
        let ghost = granted(
            StreamLeases::acquire(&store, &ask(&me, "replica-a", "t6"), 3)
                .await
                .unwrap(),
        );
        sqlx::query(
            "UPDATE stream_leases SET expires_at = now() + interval '1 second' WHERE id::text = $1",
        )
        .bind(&ghost)
        .execute(store.pool())
        .await
        .unwrap();
        StreamLeases::renew(&store, "replica-a", &[]).await.unwrap();
        let expires: OffsetDateTime =
            sqlx::query_scalar("SELECT expires_at FROM stream_leases WHERE id::text = $1")
                .bind(&ghost)
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert!(
            expires < now() + Duration::seconds(5),
            "a ghost lease was renewed"
        );

        // A replica that died stops renewing: its leases lapse and free the slots.
        sqlx::query("UPDATE stream_leases SET expires_at = now() - interval '1 second' WHERE principal = $1")
            .bind(&me)
            .execute(store.pool())
            .await
            .unwrap();
        StreamLeases::renew(&store, "replica-z", &[]).await.unwrap();
        granted(
            StreamLeases::acquire(&store, &ask(&me, "replica-a", "t5"), 1)
                .await
                .unwrap(),
        );
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

    /// The composition root wires one `PgStore` behind every port.
    #[tokio::test]
    #[expect(
        clippy::let_underscore_must_use,
        let_underscore_drop,
        reason = "each binding is a compile-time check that the store implements the port"
    )]
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

    // --- The publish outbox (ADR-0025) ------------------------------------

    #[tokio::test]
    async fn what_a_write_owes_commits_with_it_and_the_fanout_row_is_leased() {
        let store = store!();
        let (_, alice) = user();
        let channel = seeded_channel(&store, &alice).await;
        let (notification, written) = Notification::create(
            NotificationId::generate(),
            channel.clone(),
            None,
            RevisionDraft::simple(ContentDraft::titled("etcd degraded")),
            author(&alice),
            now(),
            &ChannelPolicy::default(),
        )
        .unwrap();

        let (principal_ref, key) = (alice.reference(), format!("deploy-{}", ulid_slug()));
        let claimed_at = now();
        assert_eq!(
            IdempotencyStore::claim(&store, &principal_ref, &key, claimed_at)
                .await
                .unwrap(),
            Claim::Fresh
        );
        let thumbnail = AttachmentId::generate();
        let after = notify_ports::AfterWrite {
            fanout: true,
            thumbnail: Some(thumbnail),
            sinks: vec![
                notify_domain::SinkId::new("webhook", "https://hooks.weebo.poc#0000000000000001")
                    .unwrap(),
            ],
            reply: Some(notify_ports::IdempotentReply {
                principal_ref: principal_ref.clone(),
                key: key.clone(),
                claimed_at,
                expires_at: claimed_at + Duration::hours(24),
                encode: Box::new(|cursor| Ok(format!("{{\"cursor\":\"{cursor}\"}}"))),
            }),
        };
        let cursor = notifications(&store)
            .insert_with(&notification, &written.revision, &after)
            .await
            .unwrap();

        // The replay was stored by the write, not after it.
        assert_eq!(
            IdempotencyStore::claim(&store, &principal_ref, &key, now())
                .await
                .unwrap(),
            Claim::Replayed {
                response: format!("{{\"cursor\":\"{cursor}\"}}")
            }
        );

        // So was the sink delivery, carrying the event the write appended. Read
        // directly: claiming would lease the parallel sink tests' rows too.
        // Deleted as it is read, so no later claim in this shared database meets it.
        let (event,): (serde_json::Value,) =
            sqlx::query_as("DELETE FROM sink_queue WHERE channel_id = $1 RETURNING event")
                .bind(channel.as_str())
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(
            serde_json::from_value::<notify_ports::Event>(event)
                .unwrap()
                .cursor,
            cursor
        );
        let t = now();

        // And the fan-out row: leased to one claimant, owed again once the lease
        // lapses, gone once completed.
        let outbox: &dyn notify_ports::FanoutOutbox = &store;
        let claim = |at: OffsetDateTime| async move {
            outbox
                .claim_fanout(
                    at,
                    1_000,
                    at + Duration::seconds(60),
                    10,
                    at - Duration::hours(1),
                )
                .await
                .unwrap()
                .into_iter()
                .find(|row| row.cursor == cursor)
        };
        let row = claim(t).await.expect("recorded in the write's transaction");
        assert_eq!(row.notification, notification.id);
        assert_eq!(row.attempts, 1);
        assert_eq!(row.thumbnail, Some(thumbnail));
        assert!(
            claim(t).await.is_none(),
            "two workers must not push the same notification at once"
        );
        let later = t + Duration::seconds(61);
        assert!(
            claim(later).await.is_some(),
            "a worker killed mid-push leaves the row owed"
        );
        outbox.complete_fanout(cursor).await.unwrap();
        assert!(claim(later + Duration::seconds(120)).await.is_none());
    }

    /// A publish that owes a fan-out, and the cursor its outbox row sits on.
    async fn owed_fanout(store: &PgStore) -> (NotificationId, Cursor) {
        let (_, alice) = user();
        let channel = seeded_channel(store, &alice).await;
        let (notification, written) = Notification::create(
            NotificationId::generate(),
            channel,
            None,
            RevisionDraft::simple(ContentDraft::titled("etcd degraded")),
            author(&alice),
            now(),
            &ChannelPolicy::default(),
        )
        .unwrap();
        let after = notify_ports::AfterWrite {
            fanout: true,
            ..notify_ports::AfterWrite::default()
        };
        let cursor = notifications(store)
            .insert_with(&notification, &written.revision, &after)
            .await
            .unwrap();
        (notification.id, cursor)
    }

    async fn is_dead(store: &PgStore, seq: i64) -> bool {
        sqlx::query_scalar("SELECT dead_at IS NOT NULL FROM fanout_outbox WHERE cursor_seq = $1")
            .bind(seq)
            .fetch_one(store.pool())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_fanout_row_that_keeps_failing_is_dead_lettered_and_stops_coming_back() {
        let store = store!();
        let (_, cursor) = owed_fanout(&store).await;
        let outbox: &dyn notify_ports::FanoutOutbox = &store;
        // A one-second lease that is never completed: what a worker that dies on the
        // row every time looks like from here.
        let claim = |at: OffsetDateTime| async move {
            outbox
                .claim_fanout(
                    at,
                    1_000,
                    at + Duration::seconds(1),
                    2,
                    at - Duration::hours(1),
                )
                .await
                .unwrap()
                .into_iter()
                .find(|row| row.cursor == cursor)
        };
        let t = now();

        assert_eq!(claim(t).await.map(|row| row.attempts), Some(1));
        assert_eq!(
            claim(t + Duration::seconds(2))
                .await
                .map(|row| row.attempts),
            Some(2)
        );
        assert!(
            claim(t + Duration::seconds(4)).await.is_none(),
            "two attempts were all it had"
        );
        assert!(
            is_dead(&store, i64::try_from(cursor.sequence()).unwrap()).await,
            "set aside, not lost"
        );

        // And a row behind it is not held hostage.
        let (_, next) = owed_fanout(&store).await;
        assert!(
            outbox
                .claim_fanout(
                    t + Duration::seconds(6),
                    1_000,
                    t + Duration::seconds(7),
                    2,
                    t
                )
                .await
                .unwrap()
                .iter()
                .any(|row| row.cursor == next)
        );
        outbox.complete_fanout(next).await.unwrap();
        // Dead rows are an operator's to delete; this test is that operator.
        outbox.complete_fanout(cursor).await.unwrap();
    }

    #[tokio::test]
    async fn a_fanout_row_lease_extends_and_its_deliveries_outlive_it_until_the_last_row() {
        let store = store!();
        let (id, first) = owed_fanout(&store).await;
        let outbox: &dyn notify_ports::FanoutOutbox = &store;

        // The heartbeat: the lease moves to wherever the worker says.
        let until = now() + Duration::minutes(7);
        outbox.extend_fanout(&[first], until).await.unwrap();
        let leased: Option<OffsetDateTime> =
            sqlx::query_scalar("SELECT leased_until FROM fanout_outbox WHERE cursor_seq = $1")
                .bind(i64::try_from(first.sequence()).unwrap())
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert!((leased.unwrap() - until).abs() < Duration::milliseconds(1));

        let (phone, laptop) = (DeviceId::generate(), DeviceId::generate());
        outbox.record_delivery(id, 1, phone).await.unwrap();
        outbox.record_delivery(id, 1, phone).await.unwrap();
        outbox.record_delivery(id, 2, laptop).await.unwrap();
        assert_eq!(outbox.delivered_devices(id, 1).await.unwrap(), [phone]);

        // A second revision owes a row of its own, still being worked.
        let (_, alice) = user();
        let mut notification = notifications(&store).get(id).await.unwrap().unwrap();
        let written = notification
            .revise(
                RevisionDraft::simple(ContentDraft::titled("etcd recovered")),
                author(&alice),
                now(),
                &ChannelPolicy::default(),
            )
            .unwrap();
        let after = notify_ports::AfterWrite {
            fanout: true,
            ..notify_ports::AfterWrite::default()
        };
        let second = notifications(&store)
            .append_revision_with(&notification, &written.revision, &after)
            .await
            .unwrap();

        outbox.complete_fanout(first).await.unwrap();
        assert_eq!(
            outbox.delivered_devices(id, 2).await.unwrap(),
            [laptop],
            "kept while a row of the notification is still owed"
        );
        outbox.complete_fanout(second).await.unwrap();
        for seq in [1, 2] {
            assert_eq!(
                outbox.delivered_devices(id, seq).await.unwrap(),
                [] as [DeviceId; 0],
                "gone with the last row"
            );
        }
    }

    #[tokio::test]
    async fn a_later_revision_waits_while_another_worker_pushes_the_earlier_one() {
        let store = store!();
        let (id, first) = owed_fanout(&store).await;
        let outbox: &dyn notify_ports::FanoutOutbox = &store;
        // Leased by "replica A".
        outbox
            .extend_fanout(&[first], now() + Duration::minutes(1))
            .await
            .unwrap();

        let (_, alice) = user();
        let mut notification = notifications(&store).get(id).await.unwrap().unwrap();
        let written = notification
            .revise(
                RevisionDraft::simple(ContentDraft::titled("etcd recovered")),
                author(&alice),
                now(),
                &ChannelPolicy::default(),
            )
            .unwrap();
        let after = notify_ports::AfterWrite {
            fanout: true,
            ..notify_ports::AfterWrite::default()
        };
        let second = notifications(&store)
            .append_revision_with(&notification, &written.revision, &after)
            .await
            .unwrap();

        let claim = |at: OffsetDateTime| async move {
            outbox
                .claim_fanout(
                    at,
                    1_000,
                    at + Duration::seconds(1),
                    10,
                    at - Duration::hours(1),
                )
                .await
                .unwrap()
                .into_iter()
                .any(|row| row.cursor == second)
        };
        assert!(
            !claim(now()).await,
            "replica B must not race A's older head"
        );

        // A failing earlier revision backing off is not "being pushed": it must not
        // hold the newer one back for its whole retry chain.
        outbox
            .retry_fanout(first, "relay down", now() + Duration::minutes(5))
            .await
            .unwrap();
        assert!(claim(now()).await, "blocked behind a backoff");
        outbox.complete_fanout(second).await.unwrap();
        outbox.complete_fanout(first).await.unwrap();
    }

    #[tokio::test]
    async fn an_old_dead_letter_is_purged_and_a_recent_one_kept() {
        let store = store!();
        let (_, old) = owed_fanout(&store).await;
        let (_, recent) = owed_fanout(&store).await;
        for (cursor, dead_at) in [(old, now() - Duration::days(40)), (recent, now())] {
            sqlx::query("UPDATE fanout_outbox SET dead_at = $2 WHERE cursor_seq = $1")
                .bind(i64::try_from(cursor.sequence()).unwrap())
                .bind(dead_at)
                .execute(store.pool())
                .await
                .unwrap();
        }

        // Others' old dead rows may sit in this shared database: drain them too.
        while store
            .purge_dead_fanout(now() - Duration::days(30), 1_000)
            .await
            .unwrap()
            > 0
        {}

        let left: Vec<i64> =
            sqlx::query_scalar("SELECT cursor_seq FROM fanout_outbox WHERE cursor_seq = ANY($1)")
                .bind(
                    [old, recent]
                        .map(|cursor| i64::try_from(cursor.sequence()).unwrap())
                        .to_vec(),
                )
                .fetch_all(store.pool())
                .await
                .unwrap();
        assert_eq!(left, [i64::try_from(recent.sequence()).unwrap()]);
    }

    #[tokio::test]
    async fn an_undecodable_fanout_row_is_dead_lettered_without_failing_the_claim() {
        let store = store!();
        // Below every real cursor, so it is claimed first: the worst place for it.
        let slug = ulid_slug();
        let poison = -i64::from(u32::from_str_radix(&slug[slug.len() - 6..], 36).unwrap_or(1));
        sqlx::query("INSERT INTO fanout_outbox (cursor_seq, notification) VALUES ($1, '{}')")
            .bind(poison)
            .execute(store.pool())
            .await
            .unwrap();
        let (_, good) = owed_fanout(&store).await;

        let at = now();
        let claimed = notify_ports::FanoutOutbox::claim_fanout(
            &store,
            at,
            1_000,
            at + Duration::seconds(60),
            10,
            at - Duration::hours(1),
        )
        .await
        .expect("one bad row must not fail everybody's claim");

        assert!(claimed.iter().any(|row| row.cursor == good));
        assert!(is_dead(&store, poison).await);

        notify_ports::FanoutOutbox::complete_fanout(&store, good)
            .await
            .unwrap();
        sqlx::query("DELETE FROM fanout_outbox WHERE cursor_seq = $1")
            .bind(poison)
            .execute(store.pool())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn deleting_a_notification_drops_the_push_it_still_owed() {
        let store = store!();
        let (id, cursor) = owed_fanout(&store).await;

        notifications(&store).mark_deleted(id, now()).await.unwrap();

        let left: i64 =
            sqlx::query_scalar("SELECT count(*) FROM fanout_outbox WHERE cursor_seq = $1")
                .bind(i64::try_from(cursor.sequence()).unwrap())
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(left, 0, "nobody should be buzzed about something deleted");
    }

    // --- Outbound sinks (design §7.1bis, scenario T17) ---------------------

    fn queue(store: &PgStore) -> &dyn notify_ports::SinkQueue {
        store
    }

    /// One delivery of a real event, so the row round-trips real consumer data.
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
            // Already in the redacted shape: a full-url id is what the boot-time
            // re-key test rewrites, across the whole shared table.
            sink: notify_domain::SinkId::new("webhook", "https://hooks.weebo.poc#0000000000000002")
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
        // Only this test's row: the database is shared, and a row another run left
        // due is claimed alongside it.
        let mine: Vec<_> = claimed
            .iter()
            .filter(|held| held.delivery_id == delivery.delivery_id)
            .collect();
        assert_eq!(mine.len(), 1);
        assert_eq!(
            mine[0].event, delivery.event,
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

        // A killed drain leaves the delivery owed, not lost.
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
        // To the second: column precision differs from `OffsetDateTime`.
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

        // Buried means gone from the queue.
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
            "https://hooks.weebo.poc#0000000000000002"
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
        assert_eq!(
            queue(&store).dead_letters(Some(&channel)).await.unwrap(),
            [] as [notify_ports::DeadLetter; 0]
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
    #[tokio::test]
    async fn dead_letters_past_the_retention_are_pruned_and_younger_ones_kept() {
        let store = store!();
        let old = delivery(&store).await;
        let young = delivery(&store).await;
        let now = now();

        queue(&store)
            .bury(&old, "410", now - Duration::days(40))
            .await
            .unwrap();
        queue(&store).bury(&young, "410", now).await.unwrap();

        let pruned = queue(&store)
            .prune_dead_letters(now - Duration::days(30), 1_000)
            .await
            .unwrap();
        assert!(pruned >= 1);

        let left = queue(&store)
            .dead_letters(Some(&old.channel))
            .await
            .unwrap();
        assert!(left.iter().all(|letter| letter.id != old.delivery_id));
        // Each delivery here is on a channel of its own.
        let kept = queue(&store)
            .dead_letters(Some(&young.channel))
            .await
            .unwrap();
        assert!(kept.iter().any(|letter| letter.id == young.delivery_id));
        queue(&store)
            .requeue(&young.delivery_id, now)
            .await
            .unwrap();
        queue(&store).complete(&young.delivery_id).await.unwrap();
    }

    #[tokio::test]
    async fn stored_sink_ids_are_rekeyed_in_the_queue_and_the_dead_letters() {
        // What the server's boot does with the configured urls and the key
        // (`notify-server/src/sinks.rs`, `rekey_stored`); here only the store's half:
        // every row with a mapped id moves, in both tables, and nothing else does.
        let store = store!();
        let now = now();
        let unique = ulid_slug();
        let legacy = notify_domain::SinkId::new(
            "webhook",
            &format!("https://hooks.weebo.poc/{unique}/SECRETPATH"),
        )
        .unwrap();
        let rekeyed =
            notify_domain::SinkId::new("webhook", &format!("https://hooks.weebo.poc#{unique}"))
                .unwrap();

        let queued = notify_ports::SinkDelivery {
            sink: legacy.clone(),
            ..delivery(&store).await
        };
        let buried = notify_ports::SinkDelivery {
            delivery_id: notify_domain::DeliveryId::generate(),
            ..queued.clone()
        };
        queue(&store).enqueue(&queued, now).await.unwrap();
        queue(&store).enqueue(&buried, now).await.unwrap();
        queue(&store).bury(&buried, "410", now).await.unwrap();

        let rekey = |stored: &str| (stored == legacy.as_str()).then(|| rekeyed.clone());
        assert_eq!(queue(&store).rekey_sinks(&rekey).await.unwrap(), 2);
        assert_eq!(
            queue(&store).rekey_sinks(&rekey).await.unwrap(),
            0,
            "idempotent: a second boot finds nothing under the old id"
        );

        let (in_queue,): (String,) =
            sqlx::query_as("SELECT sink FROM sink_queue WHERE delivery_id = $1")
                .bind(queued.delivery_id.to_string())
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(in_queue, rekeyed.as_str());
        let letters = queue(&store)
            .dead_letters(Some(&buried.channel))
            .await
            .unwrap();
        assert!(
            letters
                .iter()
                .any(|letter| letter.id == buried.delivery_id && letter.sink == rekeyed)
        );
        queue(&store).complete(&queued.delivery_id).await.unwrap();
    }

    #[tokio::test]
    async fn the_sink_id_migration_redacts_declaration_urls_without_a_key() {
        // `20261010200000_redacted_sink_ids.sql` cannot fingerprint (SQL never sees
        // the key), so a stored declaration's webhook url becomes origin + a keyless
        // marker. Planted in the old shape, then the migration is run again — it is
        // idempotent by design.
        const MIGRATION: &str = include_str!("../migrations/20261010200000_redacted_sink_ids.sql");

        let store = store!();
        let key = format!("redact-{}", ulid_slug());
        sqlx::query(
            "INSERT INTO managed_declarations (kind, key, snapshot, applied_at)
             VALUES ('channel', $1, $2, now())",
        )
        .bind(&key)
        .bind(serde_json::json!({ "sinks": [
            { "type": "webhook", "url": "https://hooks.slack.com/services/T0/B0/SECRETPATH" },
            { "type": "webhook", "url": "https://hooks.example#0123456789abcdef" },
            { "type": "email", "address": "ops@example.org" },
        ] }))
        .execute(store.pool())
        .await
        .unwrap();

        sqlx::raw_sql(MIGRATION)
            .execute(store.pool())
            .await
            .unwrap();

        let (snapshot,): (serde_json::Value,) = sqlx::query_as(
            "SELECT snapshot FROM managed_declarations WHERE kind = 'channel' AND key = $1",
        )
        .bind(&key)
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(
            snapshot["sinks"],
            serde_json::json!([
                { "type": "webhook", "url": "https://hooks.slack.com#redacted" },
                { "type": "webhook", "url": "https://hooks.example#0123456789abcdef" },
                { "type": "email", "address": "ops@example.org" },
            ])
        );
        sqlx::query("DELETE FROM managed_declarations WHERE kind = 'channel' AND key = $1")
            .bind(&key)
            .execute(store.pool())
            .await
            .unwrap();
    }

    // --- Retention housekeeping (design §7.2bis) ---------------------------

    async fn presign_rows(store: &PgStore, id: AttachmentId) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM attachment_presigns WHERE id = $1")
            .bind(id.to_string())
            .fetch_one(store.pool())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_presign_row_goes_once_a_revision_names_it_and_not_before() {
        let store = store!();
        let (_, alice) = user();
        let channel = seeded_channel(&store, &alice).await;

        let used = AttachmentId::generate();
        let pending = AttachmentId::generate();
        for id in [used, pending] {
            registry(&store).record(id, &channel, now()).await.unwrap();
        }
        publish_with(
            &store,
            &channel,
            &alice,
            RevisionDraft::lifecycle(
                State::Open,
                ContentDraft {
                    image: Some(Image::Attachment { reference: used }),
                    ..ContentDraft::titled("names it")
                },
            ),
            None,
            now(),
        )
        .await;

        // The suite shares one database, so other tests' referenced rows may sit
        // ahead of ours in the batch: drain rather than take one bite.
        while registry(&store).forget_referenced(1_000).await.unwrap() > 0 {}

        assert_eq!(
            presign_rows(&store, used).await,
            0,
            "referenced: no longer ours"
        );
        assert_eq!(
            presign_rows(&store, pending).await,
            1,
            "an upload on its way in keeps its row"
        );
    }

    #[tokio::test]
    async fn an_upload_is_signed_for_the_channel_that_asked_and_no_other() {
        let store = store!();
        let (_, alice) = user();
        let channel = seeded_channel(&store, &alice).await;
        let other = seeded_channel(&store, &alice).await;
        let id = AttachmentId::generate();
        registry(&store).record(id, &channel, now()).await.unwrap();

        let recent = now() - Duration::hours(1);
        assert!(
            registry(&store)
                .signed_for(id, &channel, recent)
                .await
                .unwrap()
        );
        assert!(
            !registry(&store)
                .signed_for(id, &other, recent)
                .await
                .unwrap()
        );
        assert!(
            !registry(&store)
                .signed_for(AttachmentId::generate(), &channel, recent)
                .await
                .unwrap()
        );
        assert!(
            !registry(&store)
                .signed_for(id, &channel, now() + Duration::hours(1))
                .await
                .unwrap(),
            "too old: the sweep may be collecting it"
        );
    }

    #[tokio::test]
    async fn an_attachment_stays_nameable_in_its_channel_until_its_rows_near_purge() {
        let store = store!();
        let (_, alice) = user();
        let channel = seeded_channel(&store, &alice).await;
        let other = seeded_channel(&store, &alice).await;
        let image = AttachmentId::generate();
        let (shown, _) = publish_with(
            &store,
            &channel,
            &alice,
            RevisionDraft::lifecycle(
                State::Open,
                ContentDraft {
                    image: Some(Image::Attachment { reference: image }),
                    ..ContentDraft::titled("screenshot")
                },
            ),
            None,
            now(),
        )
        .await;
        store.mark_deleted(shown.id, now()).await.unwrap();

        let named = |channel: ChannelId, deleted_after: OffsetDateTime| {
            let store = store.clone();
            async move {
                notifications(&store)
                    .attachment_in_channel(&image, &channel, deleted_after)
                    .await
                    .unwrap()
            }
        };
        assert!(
            named(channel.clone(), now() - Duration::days(6)).await,
            "deleted, but its object is kept"
        );
        assert!(!named(other, now() - Duration::days(6)).await);
        assert!(
            !named(channel, now() + Duration::hours(1)).await,
            "too close to purge"
        );
    }

    #[tokio::test]
    async fn somebodys_avatar_is_never_an_orphan_of_a_purged_notification() {
        let store = store!();
        let (alice_id, alice) = user();
        let channel = seeded_channel(&store, &alice).await;
        let avatar = AttachmentId::generate();
        let mut profile = User::from_claims(
            alice_id,
            &format!("oidc-{}", ulid_slug()),
            None,
            None,
            now(),
        )
        .unwrap();
        profile.profile.avatar = Some(avatar);
        users(&store).upsert(&profile).await.unwrap();

        let (named_it, _) = publish_with(
            &store,
            &channel,
            &alice,
            RevisionDraft::lifecycle(
                State::Open,
                ContentDraft {
                    image: Some(Image::Attachment { reference: avatar }),
                    ..ContentDraft::titled("borrowed a face")
                },
            ),
            None,
            now(),
        )
        .await;
        store
            .mark_deleted(named_it.id, now() - Duration::days(10))
            .await
            .unwrap();

        let orphaned = store
            .orphaned_attachments(now() - Duration::days(7), 1_000)
            .await
            .unwrap();
        assert!(
            !orphaned.contains(&avatar),
            "deleting it would break /avatars"
        );
    }

    #[tokio::test]
    async fn a_leader_whose_connection_dies_knows_it_no_longer_leads() {
        const JOB: &str = "conformance-lost-leader";
        let store = store!();
        let election = PgLeaderElection::new(store.pool().clone());

        let guard = election
            .try_acquire(JOB)
            .await
            .unwrap()
            .expect("nobody else leads");
        assert!(guard.is_held().await);

        // The lock's key, as `leader.rs` derives it (FNV-1a), split the way `pg_locks` shows it.
        let mut key: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in JOB.as_bytes() {
            key ^= u64::from(*byte);
            key = key.wrapping_mul(0x0000_0100_0000_01b3);
        }
        let killed: Vec<(bool,)> = sqlx::query_as(
            "SELECT pg_terminate_backend(pid) FROM pg_locks
             WHERE locktype = 'advisory' AND objsubid = 1
               AND classid::bigint = $1 AND objid::bigint = $2",
        )
        .bind(i64::try_from(key >> 32).unwrap())
        .bind(i64::try_from(key & 0xffff_ffff).unwrap())
        .fetch_all(store.pool())
        .await
        .unwrap();
        assert_eq!(killed.len(), 1, "found the leader's connection");

        assert!(!guard.is_held().await);
        assert!(
            election.try_acquire(JOB).await.unwrap().is_some(),
            "the lock went with the connection"
        );
    }

    #[tokio::test]
    async fn the_platform_lists_page_without_skipping_or_repeating() {
        let store = store!();
        let (_, alice) = user();
        let mut my_channels = Vec::new();
        for _ in 0..3 {
            my_channels.push(seeded_channel(&store, &alice).await);
        }
        let mut my_accounts = Vec::new();
        for _ in 0..3 {
            let id = ServiceAccountId::parse(&format!("bot-{}", ulid_slug())).unwrap();
            ServiceAccountRepository::upsert(
                &store,
                &ServiceAccount::new(
                    id.clone(),
                    None,
                    None,
                    notify_domain::Origin::Dynamic,
                    None,
                    now(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
            my_accounts.push(id);
        }

        // Others' rows share the database: walk every page, then look for ours, in order.
        let mut page = PageRequest::of_size(2);
        let mut seen_channels = Vec::new();
        loop {
            let listed = channels(&store).list_all(page).await.unwrap();
            seen_channels.extend(listed.items.into_iter().map(|channel| channel.id));
            let Some(next) = listed.next else { break };
            page = page.after(next);
        }
        let mut page = PageRequest::of_size(2);
        let mut seen_accounts = Vec::new();
        loop {
            let listed = accounts(&store).list_page(page).await.unwrap();
            seen_accounts.extend(listed.items.into_iter().map(|account| account.id));
            let Some(next) = listed.next else { break };
            page = page.after(next);
        }

        let mine = |seen: &[ChannelId]| -> Vec<ChannelId> {
            seen.iter()
                .filter(|id| my_channels.contains(id))
                .cloned()
                .collect()
        };
        assert_eq!(mine(&seen_channels), my_channels);
        let mine: Vec<ServiceAccountId> = seen_accounts
            .into_iter()
            .filter(|id| my_accounts.contains(id))
            .collect();
        assert_eq!(mine, my_accounts);
    }

    #[tokio::test]
    async fn silences_that_can_no_longer_apply_are_purged_and_live_ones_kept() {
        let store = store!();
        let (alice_id, alice) = user();
        let channel = seeded_channel(&store, &alice).await;
        let (shown, _) = publish(&store, &channel, &alice, "still here").await;
        let silence = |target, until| Silence {
            user: alice_id,
            target,
            until,
            since: now() - Duration::days(2),
        };
        for silenced in [
            silence(
                SilenceTarget::Group {
                    key: "lapsed".to_owned(),
                },
                Some(now() - Duration::hours(1)),
            ),
            silence(
                SilenceTarget::Group {
                    key: "running".to_owned(),
                },
                Some(now() + Duration::hours(1)),
            ),
            silence(SilenceTarget::Notification { id: shown.id }, None),
            silence(
                SilenceTarget::Notification {
                    id: NotificationId::generate(),
                },
                None,
            ),
        ] {
            store.silence(&silenced).await.unwrap();
        }

        // Others' rows share the database: drain, then look at ours.
        while store.purge_silences(now(), 1_000).await.unwrap() > 0 {}

        let left: Vec<String> = sqlx::query_scalar(
            "SELECT target_ref FROM silences WHERE user_id = $1 ORDER BY target_ref",
        )
        .bind(alice_id.to_string())
        .fetch_all(store.pool())
        .await
        .unwrap();
        assert_eq!(left.len(), 2, "{left:?}");
        assert!(
            left.iter().any(|target| target.contains("running")),
            "{left:?}"
        );
        assert!(
            left.iter()
                .any(|target| target.contains(&shown.id.to_string())),
            "{left:?}"
        );
    }

    #[tokio::test]
    async fn long_revoked_credentials_are_deleted_and_recent_ones_kept() {
        let store = store!();
        let (alice_id, alice) = user();

        let device = |label: &str| Device {
            id: DeviceId::generate(),
            owner: alice.clone(),
            label: label.to_owned(),
            transport: PushTransport::Stream,
            public_key: None,
            cursor: None,
            enrolled_at: now() - Duration::days(200),
            last_seen_at: None,
            revoked_at: None,
        };
        let (old, recent, live) = (device("old"), device("recent"), device("live"));
        for d in [&old, &recent, &live] {
            devices(&store).upsert(d).await.unwrap();
        }
        devices(&store)
            .revoke(old.id, now() - Duration::days(100))
            .await
            .unwrap();
        devices(&store).revoke(recent.id, now()).await.unwrap();

        let key = |expires_at: Option<OffsetDateTime>| ApiKey {
            id: ApiKeyId::generate(),
            prefix: ApiKeyPrefix::parse(fresh_prefix().as_str()).unwrap(),
            label: None,
            owner: ApiKeyOwner::User { id: alice_id },
            scopes: vec![Scope::Publish],
            channels: Vec::new(),
            publish_per_minute: None,
            created_at: now() - Duration::days(200),
            expires_at,
            last_used_at: None,
            revoked_at: None,
        };
        let revoked = key(None);
        let expired = key(Some(now() - Duration::days(100)));
        let active = key(Some(now() + Duration::days(10)));
        for k in [&revoked, &expired, &active] {
            keys(&store)
                .insert(&StoredApiKey {
                    key: k.clone(),
                    secret_hash: "$argon2id$stub".to_owned(),
                })
                .await
                .unwrap();
        }
        keys(&store)
            .revoke(revoked.id, now() - Duration::days(100))
            .await
            .unwrap();

        let (gone_devices, gone_keys) = store
            .prune_revoked_credentials(now() - Duration::days(90))
            .await
            .unwrap();
        assert!(gone_devices >= 1 && gone_keys >= 2);

        assert!(
            DeviceRepository::get(&store, old.id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            DeviceRepository::get(&store, recent.id)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            DeviceRepository::get(&store, live.id)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            ApiKeyRepository::get(&store, revoked.id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            ApiKeyRepository::get(&store, expired.id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            ApiKeyRepository::get(&store, active.id)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn the_retention_backlog_sees_an_expiry_nobody_has_claimed() {
        let store = store!();
        let (_, alice) = user();
        let channel = seeded_channel(&store, &alice).await;
        // Simple notifications age from creation: a month old is long past 7 days.
        publish_with(
            &store,
            &channel,
            &alice,
            RevisionDraft::simple(ContentDraft::titled("overdue")),
            None,
            now() - Duration::days(30),
        )
        .await;

        let backlog = store
            .retention_backlog(now(), now() - Duration::days(30), now() - Duration::days(7))
            .await
            .unwrap();
        assert!(backlog.pending_expiry >= 1);
        assert!(backlog.oldest_event_age_seconds > 0);
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
        // T24: empty database → full schema; reboot → zero migrations replayed. Needs its own database.
        let Some(url) = database_url() else {
            skip_without_database();
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

        // Dropped even if an assertion failed.
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

        // What `migrate_on_start = false` relies on: an empty database reports every
        // migration as owed, and a migrated one reports none.
        if store
            .pending_migrations()
            .await
            .map_err(|e| e.to_string())?
            .is_empty()
        {
            return Err("an empty database reported no pending migration".to_owned());
        }

        store.migrate().await.map_err(|e| e.to_string())?;

        let pending = store
            .pending_migrations()
            .await
            .map_err(|e| e.to_string())?;
        if !pending.is_empty() {
            return Err(format!("still pending after migrating: {pending:?}"));
        }

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

        // `installed_on` betrays a replay: a re-applied migration gets a new timestamp.
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
