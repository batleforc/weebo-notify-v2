//! The bus against a real Postgres.
//!
//! Under `mod e2e` so `task test` skips it and `task test:e2e` runs it; without a
//! database each case reports that it was skipped rather than failing.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a failing assertion is a failing test"
)]

mod e2e {
    use futures::StreamExt as _;
    use notify_domain::{ChannelId, Cursor, UserId};
    use notify_eventbus_pg::PgEventBus;
    use notify_ports::{EventBus, Poke};
    use sqlx::postgres::PgPoolOptions;

    /// A bus on the configured database, or `None` when there is not one.
    async fn bus() -> Option<PgEventBus> {
        let url = std::env::var("WNK_TEST_DATABASE_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .ok()?;

        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await
            .expect("the configured database accepts connections");

        Some(PgEventBus::new(pool))
    }

    macro_rules! bus {
        () => {
            match bus().await {
                Some(bus) => bus,
                None => {
                    eprintln!("no WNK_TEST_DATABASE_URL or DATABASE_URL — skipping");
                    return;
                }
            }
        };
    }

    /// Wait for a poke the caller recognises, ignoring whatever else is on the bus.
    ///
    /// The channel is instance-wide and the suite runs in parallel, so a test that
    /// took the first poke it saw would fail whenever another test published first.
    async fn wait_for(
        stream: &mut futures::stream::BoxStream<'static, Poke>,
        wanted: &Poke,
    ) -> bool {
        let deadline = std::time::Duration::from_secs(5);

        tokio::time::timeout(deadline, async {
            while let Some(poke) = stream.next().await {
                if poke == *wanted {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false)
    }

    #[tokio::test]
    async fn a_poke_reaches_a_subscriber_including_the_replica_that_sent_it() {
        let bus = bus!();
        let mut stream = bus.subscribe().await.unwrap();

        // Including itself is what keeps single-replica and multi-replica
        // deployments on the same code path — the case that would otherwise only
        // break in production.
        let poke = Poke::Channel {
            id: ChannelId::parse("t-fanout-self").unwrap(),
            cursor: Cursor::from_sequence(1_234),
        };
        bus.publish(&poke).await.unwrap();

        assert!(
            wait_for(&mut stream, &poke).await,
            "the sender hears itself"
        );
    }

    #[tokio::test]
    async fn every_subscriber_hears_the_same_poke() {
        let bus = bus!();
        let mut first = bus.subscribe().await.unwrap();
        let mut second = bus.subscribe().await.unwrap();

        // Two subscriptions stand in for two replicas: each holds its own stream
        // connections, so each has to learn about every publication.
        let poke = Poke::User {
            id: UserId::generate(),
            cursor: Cursor::from_sequence(7),
        };
        bus.publish(&poke).await.unwrap();

        assert!(wait_for(&mut first, &poke).await);
        assert!(wait_for(&mut second, &poke).await);
    }

    #[tokio::test]
    async fn a_poke_carries_a_cursor_and_never_a_payload() {
        let bus = bus!();
        let mut stream = bus.subscribe().await.unwrap();

        let channel = ChannelId::parse("t-fanout-cursor").unwrap();
        let poke = Poke::Channel {
            id: channel.clone(),
            cursor: Cursor::from_sequence(99),
        };
        bus.publish(&poke).await.unwrap();

        assert!(wait_for(&mut stream, &poke).await);

        // The receiver reads the authoritative row at this cursor rather than
        // trusting anything the bus carried (ADR-0008).
        assert_eq!(poke.cursor(), Some(Cursor::from_sequence(99)));
    }

    #[tokio::test]
    async fn a_listener_that_loses_its_connection_says_so_and_keeps_listening() {
        let Ok(url) =
            std::env::var("WNK_TEST_DATABASE_URL").or_else(|_| std::env::var("DATABASE_URL"))
        else {
            eprintln!("no WNK_TEST_DATABASE_URL or DATABASE_URL — skipping");
            return;
        };
        // A name of its own, so killing this listener cannot touch another test's.
        let name = format!("wnk-resync-{}", UserId::generate());
        let options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect_with(options.application_name(&name))
            .await
            .unwrap();
        let bus = PgEventBus::new(pool.clone());
        let mut stream = bus.subscribe().await.unwrap();

        sqlx::query(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
             WHERE application_name = $1 AND query ILIKE 'LISTEN%'",
        )
        .bind(&name)
        .execute(&pool)
        .await
        .unwrap();

        // Whatever was sent while it was away is gone, so every stream re-reads.
        assert!(wait_for(&mut stream, &Poke::Resync).await);

        // And the subscription lives on rather than ending at the first error.
        let poke = Poke::Channel {
            id: ChannelId::parse("t-fanout-resync").unwrap(),
            cursor: Cursor::from_sequence(7),
        };
        bus.publish(&poke).await.unwrap();
        assert!(wait_for(&mut stream, &poke).await);
    }
}
