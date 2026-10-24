//! PostgreSQL adapter (design §4); `events` is the single ordering authority (ADR-0012).
//! Columns for what is queried, `jsonb` (the domain's serde wire form) for what is read whole.
//! Plain `sqlx::query`, not the macros: every query is covered by `tests/conformance.rs` instead.
//! A state change and its event row commit in one transaction; writers do not wait on each other,
//! readers stop at the watermark (ADR-0028).

mod attachments;
mod audit;
mod channels;
mod declarations;
mod devices;
mod events;
mod federation;
mod idempotency;
mod identities;
mod interactions;
mod leader;
mod mapping;
mod notifications;
mod outbox;
mod preferences;
mod quota;
mod receipts;
mod retention;
mod sinks;

use std::time::Duration;

use notify_ports::{PortError, PortResult};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};

pub use events::EventWatermark;
pub use leader::PgLeaderElection;
pub use quota::LEASE_RENEW;

/// The migrations this adapter owns, embedded so an image needs nothing else.
pub static MIGRATIONS: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Pool limits; each one's absence fails slowly and silently (a replica that stops answering).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolSettings {
    /// Connections in the pool.
    pub max_connections: u32,
    /// How long a caller waits for a connection before failing.
    pub acquire_timeout: Duration,
    /// How long an unused connection is kept open.
    pub idle_timeout: Duration,
    /// Connection lifetime, so a failover or credential rotation is picked up without a restart.
    pub max_lifetime: Duration,
    /// Postgres `statement_timeout` for every pooled connection. Zero disables it.
    pub statement_timeout: Duration,
    /// Postgres `idle_in_transaction_session_timeout`. Zero disables it.
    pub idle_in_transaction_timeout: Duration,
}

impl Default for PoolSettings {
    fn default() -> Self {
        Self {
            max_connections: 16,
            acquire_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_mins(10),
            max_lifetime: Duration::from_mins(30),
            statement_timeout: Duration::from_secs(30),
            idle_in_transaction_timeout: Duration::from_secs(60),
        }
    }
}

/// What a readiness probe learned about the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// A connection answered `SELECT 1`.
    Answered,
    /// Every connection is checked out, so none was taken to ask (see [`PgStore::probe`]).
    Saturated,
}

/// Every repository over one pool: several must append events in the same transaction.
#[derive(Debug, Clone)]
pub struct PgStore {
    pool: PgPool,
}

impl PgStore {
    /// Wrap an existing pool.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Open a pool against `url` with the default limits.
    ///
    /// # Errors
    ///
    /// Fails if the database cannot be reached or the URL does not parse.
    pub async fn connect(url: &str, max_connections: u32) -> PortResult<Self> {
        Self::connect_with(
            url,
            &PoolSettings {
                max_connections,
                ..PoolSettings::default()
            },
        )
        .await
    }

    /// Open a pool against `url`.
    ///
    /// The two server-side timeouts are sent as startup parameters rather than
    /// `SET` in an `after_connect` hook: they are then the connection's *default*,
    /// so a `RESET` inside some query cannot drop them, and the one place that
    /// needs them lifted ([`Self::migrate`]) does it explicitly.
    ///
    /// # Errors
    ///
    /// Fails if the database cannot be reached or the URL does not parse.
    pub async fn connect_with(url: &str, settings: &PoolSettings) -> PortResult<Self> {
        let options: PgConnectOptions = url
            .parse()
            .map_err(|error| PortError::unavailable("postgres", error))?;
        let options = options.options([
            (
                "statement_timeout",
                format!("{}ms", settings.statement_timeout.as_millis()),
            ),
            (
                "idle_in_transaction_session_timeout",
                format!("{}ms", settings.idle_in_transaction_timeout.as_millis()),
            ),
        ]);

        let pool = PgPoolOptions::new()
            .max_connections(settings.max_connections)
            .acquire_timeout(settings.acquire_timeout)
            .idle_timeout(settings.idle_timeout)
            .max_lifetime(settings.max_lifetime)
            .connect_with(options)
            .await
            .map_err(|error| PortError::unavailable("postgres", error))?;

        Ok(Self::new(pool))
    }

    /// On its own connection with `statement_timeout` lifted (closed afterwards), so a long
    /// migration is not killed mid-deploy and the lifted limit never reaches a request.
    ///
    /// # Errors
    ///
    /// Fails if a migration cannot be applied.
    pub async fn migrate(&self) -> PortResult<()> {
        let mut connection = self
            .pool
            .acquire()
            .await
            .map_err(|error| PortError::unavailable("postgres", error))?;
        connection.close_on_drop();

        sqlx::query("SET statement_timeout = 0")
            .execute(&mut *connection)
            .await
            .map_err(|error| PortError::unavailable("postgres", error))?;

        MIGRATIONS
            .run(&mut *connection)
            .await
            .map_err(|error| PortError::unavailable("postgres", error))?;

        // After a logical restore into a fresh cluster the transaction counter
        // starts over, and the event log's seqs with it (ADR-0028). Cheap when
        // nothing is wrong, which is every run but that one.
        let rebased: bool = sqlx::query_scalar("SELECT events_rebase()")
            .fetch_one(&mut *connection)
            .await
            .map_err(|error| PortError::unavailable("postgres", error))?;
        if rebased {
            tracing::warn!(
                "the transaction counter went backwards (a logical restore?); event seqs re-based above the log"
            );
        }
        Ok(())
    }

    /// The versions this binary knows and the database has not applied.
    ///
    /// Empty means the schema is current — or *ahead*, during a rolling deploy
    /// where a newer replica already migrated, which is the case expand/contract
    /// migrations exist for and not one to refuse. Read-only: it never creates the
    /// bookkeeping table, so it is safe to ask with `migrate_on_start = false`.
    ///
    /// # Errors
    ///
    /// Fails if the database cannot be asked.
    pub async fn pending_migrations(&self) -> PortResult<Vec<i64>> {
        let tracked: bool =
            sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations') IS NOT NULL")
                .fetch_one(&self.pool)
                .await
                .map_err(|error| PortError::unavailable("postgres", error))?;

        let applied: Vec<i64> = if tracked {
            sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success")
                .fetch_all(&self.pool)
                .await
                .map_err(|error| PortError::unavailable("postgres", error))?
        } else {
            Vec::new()
        };

        Ok(MIGRATIONS
            .iter()
            .filter(|migration| migration.migration_type.is_up_migration())
            .map(|migration| migration.version)
            .filter(|version| !applied.contains(version))
            .collect())
    }

    /// The pool, for the composition root's health check and the event bus.
    #[must_use]
    pub const fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Fails when the database cannot answer.
    pub async fn ping(&self) -> PortResult<()> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(|error| PortError::unavailable("postgres", error))
    }

    /// Answer `/readyz` without queueing behind the traffic it is judging.
    ///
    /// When every connection is checked out the probe does not wait for one: a
    /// saturated pool is a replica that is *busy*, and a probe that queued would
    /// time out on every replica at the same moment and take the whole fleet out
    /// of the load balancer during a burst — turning load into an outage. A
    /// database that is actually gone still fails here, because a pool with any
    /// free or idle slot is asked directly, and the requests holding a saturated
    /// pool fail on their own `statement_timeout` and free it.
    ///
    /// # Errors
    ///
    /// Fails when a connection could be had and the database did not answer.
    pub async fn probe(&self) -> PortResult<Probe> {
        let saturated = self.pool.num_idle() == 0
            && self.pool.size() >= self.pool.options().get_max_connections();
        if saturated {
            return Ok(Probe::Saturated);
        }
        self.ping().await.map(|()| Probe::Answered)
    }
}
