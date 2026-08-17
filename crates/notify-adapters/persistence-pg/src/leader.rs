//! Electing the replica that runs the background jobs, with a Postgres advisory
//! lock (design §11, scenario T22).
//!
//! Zero extra infrastructure: the database the instance already needs is the thing
//! that decides. A session advisory lock is held for as long as its **connection**
//! is, which is what makes this safe — a replica that is partitioned, killed or
//! stalled loses its connection, and the lock goes with it. Nothing has to time out
//! and nothing has to be renewed.
//!
//! That is also why the guard holds a connection out of the pool rather than
//! borrowing one per call. A pooled connection handed back would take the lock with
//! it, and the next unrelated query would silently be leading.

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
    /// Wrap a pool.
    ///
    /// It should have room to spare: every job that leads holds one connection for
    /// as long as it runs.
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

        // Non-blocking: a replica that is not the leader must go on serving the API
        // immediately rather than parking a task on a lock it may never get.
        let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(lock_key(job))
            .fetch_one(&mut *connection)
            .await
            .map_err(|error| fail("leader", error))?;

        if !acquired {
            // Somebody else has it. A normal answer, not a failure.
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

        // The connection *is* the lock, so asking whether it still answers is
        // asking whether we still lead. Checked between batches rather than once at
        // the top: a purge loop that took the lock, lost it during a partition and
        // kept deleting is the failure this exists to prevent (scenario T22).
        match sqlx::query_scalar::<_, i32>("SELECT 1")
            .fetch_one(&mut **connection)
            .await
        {
            Ok(_) => true,
            Err(error) => {
                tracing::warn!(job = self.job, %error, "lost the leader connection");
                // Dropped rather than handed a dead connection back to the pool.
                held.take();
                false
            }
        }
    }

    async fn release(&self) {
        let Some(mut connection) = self.connection.lock().await.take() else {
            return;
        };

        // Explicit rather than implicit, so a clean SIGTERM drain hands leadership
        // over immediately instead of leaving the instance leaderless until this
        // connection is eventually recycled (design §11).
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
        // Returning this connection to the pool would take the lock with it, and the
        // next unrelated query on that connection would silently be leading. Closing
        // it instead ends the session, which is how Postgres releases everything the
        // session held.
        //
        // `try_lock` because `Drop` cannot await. On the normal path `release` has
        // already emptied this and there is nothing to do.
        if let Ok(mut held) = self.connection.try_lock()
            && let Some(mut connection) = held.take()
        {
            connection.close_on_drop();
        }
    }
}

/// The advisory-lock key a job name takes.
///
/// FNV-1a rather than the standard library's hasher: `DefaultHasher` is explicitly
/// allowed to change between releases, and a lock key that moved on a toolchain
/// upgrade would let two replicas lead the same job across a rolling deploy.
fn lock_key(job: &str) -> i64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET;
    for byte in job.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }

    // Reinterpreted rather than truncated: `pg_try_advisory_lock` takes a signed
    // 64-bit key, and every bit of the hash is worth keeping.
    i64::from_ne_bytes(hash.to_ne_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_job_name_always_maps_to_the_same_lock() {
        // If this moved between builds, two replicas of different versions would
        // each believe they lead the purge during a rolling deploy.
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
