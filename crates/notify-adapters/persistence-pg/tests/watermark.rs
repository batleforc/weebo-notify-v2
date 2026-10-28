//! The event watermark (ADR-0028), against a real Postgres: writers no longer wait
//! on each other, so rows commit out of seq order, and no reader may step over one
//! that has not committed yet.
//!
//! The watermark is cluster-wide — any transaction anywhere on the server holds it
//! back — so these assert *eventual* delivery against a deadline rather than
//! timing, and they run alone (`.config/nextest.toml`): each holds a transaction
//! open on purpose, which delays every other test's reads while it does.

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
    use std::time::Duration;

    use notify_domain::{Channel, ChannelId, Cursor, Membership, NotificationId, Scope, UserId};
    use notify_persistence_pg::PgStore;
    use notify_ports::{ChannelRepository, EventPayload, EventStore, PageRequest, SyncQuery};
    use sqlx::{Postgres, Transaction};
    use time::OffsetDateTime;

    /// Long enough for any ordinary transaction elsewhere on the server to end.
    const EVENTUALLY: Duration = Duration::from_secs(30);

    async fn store(connections: u32) -> Option<PgStore> {
        let url = std::env::var("WNK_TEST_DATABASE_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .ok()?;
        let store = PgStore::connect(&url, connections)
            .await
            .expect("the configured database accepts connections");
        store.migrate().await.expect("migrations apply");
        Some(store)
    }

    macro_rules! store {
        ($connections:expr) => {
            match store($connections).await {
                Some(store) => store,
                None => {
                    assert!(
                        std::env::var_os("WNK_TEST_REQUIRE_DB").is_none(),
                        "WNK_TEST_REQUIRE_DB is set but no database is configured — refusing to skip"
                    );
                    eprintln!("no WNK_TEST_DATABASE_URL or DATABASE_URL — skipping");
                    return;
                }
            }
        };
    }

    /// A channel of its own, with `reader` subscribed.
    async fn channel_for(store: &PgStore, reader: UserId) -> ChannelId {
        let slug = NotificationId::generate()
            .to_string()
            .trim_start_matches("ntf_")
            .to_lowercase();
        let id = ChannelId::parse(&format!("t-{slug}")).unwrap();
        let mut channel = Channel::dynamic(id.clone(), None).unwrap();
        channel.upsert_member(
            Membership::new(
                notify_domain::Principal::User { id: reader },
                vec![Scope::Subscribe],
            )
            .unwrap(),
        );
        ChannelRepository::upsert(store, &channel).await.unwrap();
        id
    }

    /// Append one event inside `transaction`, exactly as the repositories do: the
    /// column default picks the seq.
    async fn append(transaction: &mut Transaction<'_, Postgres>, channel: &ChannelId) -> i64 {
        let payload = EventPayload::Closure {
            notification_id: NotificationId::generate(),
            closed_at: OffsetDateTime::now_utc(),
            closed_by: None,
        };
        sqlx::query_scalar(
            "INSERT INTO events (audience_kind, channel_id, recorded_at, kind, notification_id, payload)
             VALUES ('channel', $1, now(), $2, $3, $4)
             RETURNING seq",
        )
        .bind(channel.as_str())
        .bind(payload.kind().as_str())
        .bind(payload.notification_id().map(|id| id.to_string()))
        .bind(serde_json::to_value(&payload).unwrap())
        .fetch_one(&mut **transaction)
        .await
        .unwrap()
    }

    /// What `reader` is served after `after`, as seqs.
    async fn served(store: &PgStore, reader: UserId, after: i64) -> Vec<i64> {
        let page = EventStore::sync(
            store,
            reader,
            &SyncQuery {
                after: Some(Cursor::from_sequence(u64::try_from(after).unwrap())),
                channels: Vec::new(),
                groups: Vec::new(),
                page: PageRequest::first(),
            },
        )
        .await
        .unwrap();
        page.events
            .iter()
            .map(|event| i64::try_from(event.cursor.sequence()).unwrap())
            .collect()
    }

    /// Read until `done` holds, or fail at the deadline.
    async fn eventually(
        store: &PgStore,
        reader: UserId,
        after: i64,
        done: impl Fn(&[i64]) -> bool,
    ) -> Vec<i64> {
        let deadline = std::time::Instant::now() + EVENTUALLY;
        loop {
            let seqs = served(store, reader, after).await;
            if done(&seqs) {
                return seqs;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "still not served after {EVENTUALLY:?}: {seqs:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn critical_no_reader_skips_an_event_committed_out_of_order() {
        let store = store!(4);
        let reader = UserId::generate();
        let channel = channel_for(&store, reader).await;

        // A takes the lower seq and does not commit; B takes the next and does.
        let mut slow = store.pool().begin().await.unwrap();
        let first = append(&mut slow, &channel).await;
        let mut fast = store.pool().begin().await.unwrap();
        let second = append(&mut fast, &channel).await;
        fast.commit().await.unwrap();
        assert!(second > first, "a later transaction takes a later seq");

        // Served `second` now, a reader would move its cursor past `first` and
        // never see it. The read either waits or answers without it.
        if let Ok(early) = tokio::time::timeout(
            Duration::from_millis(500),
            served(&store, reader, first - 1),
        )
        .await
        {
            assert!(
                !early.contains(&second),
                "a row was served while an older transaction could still commit below it"
            );
        }
        let head = EventStore::head(&store).await.unwrap();
        assert!(
            head.sequence() < u64::try_from(first).unwrap(),
            "\"start from now\" must not be a cursor past a row still in flight"
        );

        slow.commit().await.unwrap();
        let all = eventually(&store, reader, first - 1, |seqs| seqs.len() == 2).await;
        assert_eq!(all, [first, second], "both, in seq order");
    }

    #[tokio::test]
    async fn critical_a_transaction_that_started_first_sorts_first_even_if_it_appends_last() {
        // The case that sinks "serve rows whose xid is below xmin, page by a
        // sequence": the early transaction draws the *later* sequence value and
        // commits first, the reader moves past it, and the late one lands below.
        // With seq derived from the transaction id, the early one sorts first.
        let store = store!(4);
        let reader = UserId::generate();
        let channel = channel_for(&store, reader).await;

        let mut early = store.pool().begin().await.unwrap();
        sqlx::query("SELECT pg_current_xact_id()")
            .execute(&mut *early)
            .await
            .unwrap();
        let mut late = store.pool().begin().await.unwrap();
        let held = append(&mut late, &channel).await;
        let committed = append(&mut early, &channel).await;
        early.commit().await.unwrap();
        assert!(
            committed < held,
            "seq follows the transaction, not the insert"
        );

        let seen = eventually(&store, reader, committed - 1, |seqs| !seqs.is_empty()).await;
        assert_eq!(seen, [committed]);

        // The reader's cursor is now `committed`, and the open transaction's row
        // still lands after it.
        late.commit().await.unwrap();
        let rest = eventually(&store, reader, committed, |seqs| !seqs.is_empty()).await;
        assert_eq!(rest, [held]);
    }

    #[tokio::test]
    async fn the_head_never_passes_a_writer_that_has_not_committed() {
        let store = store!(4);
        let reader = UserId::generate();
        let channel = channel_for(&store, reader).await;

        let mut writer = store.pool().begin().await.unwrap();
        let pending = append(&mut writer, &channel).await;
        let pending = u64::try_from(pending).unwrap();

        // Without waiting on the writer, unlike the lock it replaces.
        let head = tokio::time::timeout(Duration::from_secs(1), EventStore::head(&store))
            .await
            .expect("the head does not wait on writers")
            .unwrap();
        assert!(head.sequence() < pending);

        writer.commit().await.unwrap();
        let deadline = std::time::Instant::now() + EVENTUALLY;
        while EventStore::head(&store).await.unwrap().sequence() < pending {
            assert!(
                std::time::Instant::now() < deadline,
                "the head never caught up"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn a_read_held_back_too_long_serves_the_safe_prefix_after_its_wait() {
        let store = store!(4);
        let reader = UserId::generate();
        let channel = channel_for(&store, reader).await;

        let mut first = store.pool().begin().await.unwrap();
        let served_already = append(&mut first, &channel).await;
        first.commit().await.unwrap();
        eventually(&store, reader, served_already - 1, |seqs| !seqs.is_empty()).await;

        let mut holding = store.pool().begin().await.unwrap();
        append(&mut holding, &channel).await;
        let mut behind = store.pool().begin().await.unwrap();
        append(&mut behind, &channel).await;
        behind.commit().await.unwrap();

        let started = std::time::Instant::now();
        let seqs = served(&store, reader, served_already).await;
        let waited = started.elapsed();

        assert!(seqs.is_empty(), "nothing past the held row: {seqs:?}");
        assert!(
            waited >= Duration::from_millis(4_500) && waited < Duration::from_secs(15),
            "waited {waited:?}, not about the 5 s the read allows itself"
        );
        holding.rollback().await.unwrap();
    }

    #[tokio::test]
    async fn a_held_back_row_is_measured() {
        let store = store!(4);
        let reader = UserId::generate();
        let channel = channel_for(&store, reader).await;

        let mut holding = store.pool().begin().await.unwrap();
        append(&mut holding, &channel).await;
        let mut behind = store.pool().begin().await.unwrap();
        let held = append(&mut behind, &channel).await;
        behind.commit().await.unwrap();

        let reading = store.event_watermark().await.unwrap();
        assert!(reading.watermark.sequence() < u64::try_from(held).unwrap());
        assert!(reading.newest.sequence() >= u64::try_from(held).unwrap());
        assert!(
            reading.oldest_held_back.is_some(),
            "a committed row past the watermark is what the lag gauge reports"
        );

        holding.rollback().await.unwrap();
    }

    #[tokio::test]
    async fn a_transaction_counter_that_went_backwards_is_rebased_on_migrate() {
        // What a `pg_dump` restored into a fresh cluster looks like from here: rows
        // whose seqs carry transaction ids from a counter far ahead of this one.
        let store = store!(4);
        let reader = UserId::generate();
        let channel = channel_for(&store, reader).await;

        let mut restored = store.pool().begin().await.unwrap();
        let ahead: i64 = sqlx::query_scalar(
            "SELECT events_seq_base() + (pg_current_xact_id()::text::bigint + 1000) * 65536",
        )
        .fetch_one(&mut *restored)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO events (seq, audience_kind, channel_id, recorded_at, kind)
             VALUES ($1, 'channel', $2, now(), 'expired_unseen')",
        )
        .bind(ahead)
        .bind(channel.as_str())
        .execute(&mut *restored)
        .await
        .unwrap();
        restored.commit().await.unwrap();

        store.migrate().await.unwrap();

        let mut writer = store.pool().begin().await.unwrap();
        let next = append(&mut writer, &channel).await;
        writer.commit().await.unwrap();
        assert!(next > ahead, "a new row sorts after the restored log");
        assert!(
            EventStore::head(&store).await.unwrap().sequence() >= u64::try_from(ahead).unwrap(),
            "and the watermark is back above it"
        );
        let rebased_again: bool = sqlx::query_scalar("SELECT events_rebase()")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert!(!rebased_again, "a healthy log is left alone");
    }

    /// Rough throughput of event-appending transactions, with and without the
    /// global advisory lock this crate used to take (ADR-0028). Not a gate:
    /// `cargo nextest run -p notify-persistence-pg --run-ignored only -E 'test(bench_)' --no-capture`.
    #[tokio::test]
    #[ignore = "a measurement, not a test"]
    async fn bench_event_append_throughput() {
        let store = store!(40);
        let reader = UserId::generate();
        let channel = channel_for(&store, reader).await;

        for writers in [1_usize, 4, 16, 32] {
            for locked in [true, false] {
                let stop = std::time::Instant::now() + Duration::from_secs(3);
                let tasks: Vec<_> = (0..writers)
                    .map(|_| {
                        let pool = store.pool().clone();
                        let channel = channel.clone();
                        tokio::spawn(async move {
                            let mut done = 0_u64;
                            while std::time::Instant::now() < stop {
                                let mut transaction = pool.begin().await.unwrap();
                                if locked {
                                    sqlx::query("SELECT pg_advisory_xact_lock($1, $2)")
                                        .bind(0x0057_4e4b_i32)
                                        .bind(1_i32)
                                        .execute(&mut *transaction)
                                        .await
                                        .unwrap();
                                }
                                // A publish is about this shape: a row locked, an
                                // event, a few more writes, one commit.
                                append(&mut transaction, &channel).await;
                                sqlx::query("SELECT pg_sleep(0.001)")
                                    .execute(&mut *transaction)
                                    .await
                                    .unwrap();
                                transaction.commit().await.unwrap();
                                done += 1;
                            }
                            done
                        })
                    })
                    .collect();
                let mut total = 0;
                for task in tasks {
                    total += task.await.unwrap();
                }
                println!(
                    "writers={writers:>2} {:<9} {:>7.0} tx/s",
                    if locked { "lock" } else { "watermark" },
                    f64::from(u32::try_from(total).unwrap_or(u32::MAX)) / 3.0
                );
            }
        }
    }
}
