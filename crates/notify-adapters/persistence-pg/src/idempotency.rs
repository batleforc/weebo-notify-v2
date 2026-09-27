//! Replaying a request instead of repeating it (design §8, scenario T16).
//!
//! The reservation is two-phase, and the first phase is what makes it work. A
//! single "store the response when done" would leave a window in which a retry
//! arriving mid-flight starts a second publish — which is exactly the case retries
//! are made of, a client that timed out and tried again. Claiming the key first
//! turns that race into a `409`.

use notify_ports::{Claim, IdempotencyStore, PortResult};
use time::OffsetDateTime;

use crate::PgStore;
use crate::mapping::fail;

/// How long a claim stays in flight before another request may take it over.
///
/// This is the answer to "the replica holding this key died mid-publish". Long
/// enough that a slow publish is not stolen from underneath itself, short enough
/// that a client retrying after a crash is not told `409` for the whole
/// twenty-four-hour replay window.
const IN_FLIGHT: time::Duration = time::Duration::minutes(5);

#[async_trait::async_trait]
impl IdempotencyStore for PgStore {
    async fn claim(
        &self,
        principal_ref: &str,
        key: &str,
        now: OffsetDateTime,
    ) -> PortResult<Claim> {
        // One statement, so two requests arriving together cannot both come back
        // `Fresh`. The `WHERE expires_at <= $3` on the conflict branch is what makes
        // a lapsed reservation reusable rather than a permanent tombstone.
        let claimed: Option<(bool, Option<String>)> = sqlx::query_as(
            "INSERT INTO idempotency_keys (principal_ref, key, response, claimed_at, expires_at)
             VALUES ($1, $2, NULL, $3, $4)
             ON CONFLICT (principal_ref, key) DO UPDATE
                 SET claimed_at = EXCLUDED.claimed_at,
                     expires_at = EXCLUDED.expires_at,
                     response   = NULL
               WHERE idempotency_keys.expires_at <= $3
             RETURNING true AS fresh, response",
        )
        .bind(principal_ref)
        .bind(key)
        .bind(now)
        .bind(now + IN_FLIGHT)
        .fetch_optional(self.pool())
        .await
        .map_err(|error| fail("idempotency", error))?;

        if claimed.is_some() {
            return Ok(Claim::Fresh);
        }

        // The conflict branch did not fire, so somebody else holds it. Whether they
        // finished is the difference between a replay and a `409`.
        let existing: Option<Option<String>> = sqlx::query_scalar(
            "SELECT response FROM idempotency_keys WHERE principal_ref = $1 AND key = $2",
        )
        .bind(principal_ref)
        .bind(key)
        .fetch_optional(self.pool())
        .await
        .map_err(|error| fail("idempotency", error))?;

        Ok(match existing.flatten() {
            Some(response) => Claim::Replayed { response },
            // Answered with `409` and a `Retry-After`, not by waiting: holding a
            // request open on the chance another one finishes is how a thread pool
            // is spent.
            None => Claim::InFlight,
        })
    }

    async fn complete(
        &self,
        principal_ref: &str,
        key: &str,
        claimed_at: OffsetDateTime,
        response: &str,
        expires_at: OffsetDateTime,
    ) -> PortResult<()> {
        // `claimed_at` is the claim's identity: a takeover rewrites it, so a slow
        // original that finishes late matches nothing and cannot clobber the
        // response its successor stored.
        sqlx::query(
            "UPDATE idempotency_keys
                SET response = $4, expires_at = $5
              WHERE principal_ref = $1 AND key = $2 AND claimed_at = $3 AND response IS NULL",
        )
        .bind(principal_ref)
        .bind(key)
        .bind(claimed_at)
        .bind(response)
        .bind(expires_at)
        .execute(self.pool())
        .await
        .map_err(|error| fail("idempotency", error))
        .map(|_| ())
    }

    async fn release(
        &self,
        principal_ref: &str,
        key: &str,
        claimed_at: OffsetDateTime,
    ) -> PortResult<()> {
        // Only an unfinished claim, and only ours: releasing a completed one would
        // delete a response somebody is entitled to replay, and releasing one taken
        // over would let a third request publish alongside the second.
        sqlx::query(
            "DELETE FROM idempotency_keys
             WHERE principal_ref = $1 AND key = $2 AND claimed_at = $3 AND response IS NULL",
        )
        .bind(principal_ref)
        .bind(key)
        .bind(claimed_at)
        .execute(self.pool())
        .await
        .map_err(|error| fail("idempotency", error))
        .map(|_| ())
    }

    async fn prune(&self, now: OffsetDateTime) -> PortResult<u64> {
        let pruned = sqlx::query("DELETE FROM idempotency_keys WHERE expires_at <= $1")
            .bind(now)
            .execute(self.pool())
            .await
            .map_err(|error| fail("idempotency", error))?;

        Ok(pruned.rows_affected())
    }
}
