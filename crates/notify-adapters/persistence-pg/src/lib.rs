//! PostgreSQL adapter (design §4). The `events` table is the single ordering
//! authority (ADR-0012).
//!
//! # How the schema is shaped
//!
//! Columns for what is queried, `jsonb` for what is only ever read back whole.
//! Filters, orderings and joins get real columns with real indexes; content,
//! actions, policy and authors are stored in the domain's own serde
//! representation. That representation is already the published wire contract
//! (ADR-0013), so this introduces no second format to keep in step.
//!
//! # Why the queries are not `sqlx::query!`
//!
//! The macros check SQL at compile time against a live database, and that is worth
//! something. It is not free: it makes `.sqlx` metadata a build input that has to
//! be regenerated whenever a query changes, and it fights hard with the shapes this
//! adapter is made of — `jsonb` round-trips, `text[]` filters, and reads whose
//! `WHERE` clause is assembled from optional filters.
//!
//! The trade taken here is that **every query in this crate is exercised against a
//! real Postgres by `tests/conformance.rs`**, which walks the whole port surface.
//! A typo fails a test rather than a build, and the workspace compiles with no
//! database anywhere. If that suite ever stops covering a query, this decision
//! stops paying for itself — so a new method here comes with a test there.
//!
//! # Transactions
//!
//! A state change and its event row are written together or not at all (ADR-0012).
//! Every method that returns a [`notify_domain::Cursor`] opens a transaction, does
//! its work, appends to `events`, and commits — so "the event log and the data
//! disagree" is not a reachable state.

mod audit;
mod channels;
mod devices;
mod events;
mod idempotency;
mod identities;
mod interactions;
mod leader;
mod mapping;
mod notifications;
mod quota;
mod receipts;

use notify_ports::{PortError, PortResult};
use sqlx::postgres::{PgPool, PgPoolOptions};

pub use leader::PgLeaderElection;

/// The migrations this adapter owns, embedded in the binary.
///
/// Embedded rather than read from disk so a container image needs nothing but
/// itself to bring a fresh database up to date.
pub static MIGRATIONS: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Every repository, over one connection pool.
///
/// One type implementing many ports rather than one per port: they share a pool
/// and, more importantly, several of them have to write inside *the same*
/// transaction as an event append. Splitting them would mean either sharing a
/// transaction across objects or giving up the guarantee.
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

    /// Bring the schema up to date.
    ///
    /// # Errors
    ///
    /// Fails if a migration cannot be applied, which is an operator problem rather
    /// than something to start serving traffic on top of.
    pub async fn migrate(&self) -> PortResult<()> {
        MIGRATIONS
            .run(&self.pool)
            .await
            .map_err(|error| PortError::unavailable("postgres", error))
    }

    /// The pool, for the composition root's own health check and for the event bus,
    /// which listens on the same database.
    #[must_use]
    pub const fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Answer `/readyz` (design §13).
    ///
    /// # Errors
    ///
    /// Fails when the database cannot answer, which is exactly what `/readyz` is
    /// asking about.
    pub async fn ping(&self) -> PortResult<()> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(|error| PortError::unavailable("postgres", error))
    }
}
