//! PostgreSQL adapter (design §4); `events` is the single ordering authority (ADR-0012).
//! Columns for what is queried, `jsonb` (the domain's serde wire form) for what is read whole.
//! Plain `sqlx::query`, not the macros: every query is covered by `tests/conformance.rs` instead.
//! A state change and its event row commit in one transaction.

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
mod preferences;
mod quota;
mod receipts;
mod sinks;

use notify_ports::{PortError, PortResult};
use sqlx::postgres::{PgPool, PgPoolOptions};

pub use leader::PgLeaderElection;
pub use quota::LEASE_RENEW;

/// The migrations this adapter owns, embedded so an image needs nothing else.
pub static MIGRATIONS: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

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

    /// Open a pool against `url`.
    ///
    /// # Errors
    ///
    /// Fails if the database cannot be reached or the URL does not parse.
    pub async fn connect(url: &str, max_connections: u32) -> PortResult<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .connect(url)
            .await
            .map_err(|error| PortError::unavailable("postgres", error))?;

        Ok(Self::new(pool))
    }

    /// Fails if a migration cannot be applied.
    pub async fn migrate(&self) -> PortResult<()> {
        MIGRATIONS
            .run(&self.pool)
            .await
            .map_err(|error| PortError::unavailable("postgres", error))
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
}
