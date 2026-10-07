//! Leader election for background jobs via a Postgres session advisory lock (design §11, T22).
//! The lock lives as long as its dedicated connection, so a dead or partitioned replica loses it;
//! never return that connection to the pool, or an unrelated query would silently be leading.

use std::sync::Arc;

use notify_ports::{LeaderElection, LeaderGuard, PortResult};
use sqlx::pool::PoolConnection;
use sqlx::{PgPool, Postgres};
use tokio::sync::Mutex;

use crate::mapping::fail;

/// Leadership over a Postgres advisory lock.
#[derive(Debug, Clone)]
pub struct PgLeaderElection {
    pool: PgPool,
}

impl PgLeaderElection {
    /// Wrap a pool; each leading job holds one connection while it runs.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl LeaderElection for PgLeaderElection {
    async fn try_acquire(&self, job: &'static str) -> PortResult<Option<Box<dyn LeaderGuard>>> {
        let mut connection = self
            .pool
            .acquire()
            .await
            .map_err(|error| fail("leader", error))?;

        // Non-blocking, so followers keep serving immediately.
        let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(lock_key(job))
            .fetch_one(&mut *connection)
            .await
            .map_err(|error| fail("leader", error))?;

        if !acquired {
            // Somebody else leads: a normal answer.
            return Ok(None);
        }

        tracing::debug!(job, "took the leader lock");

        Ok(Some(Box::new(PgLeaderGuard {
            job,
            connection: Arc::new(Mutex::new(Some(connection))),
        })))
    }
}

/// Proof that this replica leads `job`, for as long as it holds the connection.
struct PgLeaderGuard {
    job: &'static str,
    connection: Arc<Mutex<Option<PoolConnection<Postgres>>>>,
}

impl core::fmt::Debug for PgLeaderGuard {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PgLeaderGuard")
            .field("job", &self.job)
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl LeaderGuard for PgLeaderGuard {
    async fn is_held(&self) -> bool {
        let mut held = self.connection.lock().await;
        let Some(connection) = held.as_mut() else {
            return false;
        };

        // The connection is the lock: checked between batches so a partitioned leader stops (T22).
        match sqlx::query_scalar::<_, i32>("SELECT 1")
            .fetch_one(&mut **connection)
            .await
        {
            Ok(_) => true,
            Err(error) => {
                tracing::warn!(job = self.job, %error, "lost the leader connection");
                // Dropped, never handed back to the pool.
                held.take();
                false
            }
        }
    }

    async fn release(&self) {
        let Some(mut connection) = self.connection.lock().await.take() else {
            return;
        };

        // Explicit so a SIGTERM drain hands over leadership immediately (§11).
        if let Err(error) = sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(lock_key(self.job))
            .execute(&mut *connection)
            .await
        {
            tracing::warn!(job = self.job, %error, "could not release the leader lock");
        } else {
            tracing::debug!(job = self.job, "released the leader lock");
        }
    }
}

impl Drop for PgLeaderGuard {
    fn drop(&mut self) {
        // Closing (not pooling) the connection releases the lock. `try_lock` since `Drop` cannot await.
        if let Ok(mut held) = self.connection.try_lock()
            && let Some(mut connection) = held.take()
        {
            connection.close_on_drop();
        }
    }
}

/// The advisory-lock key for a job: FNV-1a, stable across toolchains unlike `DefaultHasher`.
fn lock_key(job: &str) -> i64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET;
    for byte in job.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }

    // Reinterpreted as the signed key Postgres takes, keeping every bit.
    i64::from_ne_bytes(hash.to_ne_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_job_name_always_maps_to_the_same_lock() {
        // A moving key would let two versions both lead during a rolling deploy.
        assert_eq!(lock_key("purge"), lock_key("purge"));
        assert_ne!(lock_key("purge"), lock_key("reconcile"));
    }

    #[test]
    fn different_jobs_never_contend_with_each_other() {
        let keys = ["purge", "reconcile", "sinks", "digests"].map(lock_key);

        let mut seen = keys.to_vec();
        seen.sort_unstable();
        seen.dedup();

        assert_eq!(
            seen.len(),
            keys.len(),
            "a collision here serialises two jobs"
        );
    }
}
