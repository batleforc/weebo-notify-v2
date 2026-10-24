//! Replaying a request instead of repeating it (design §8, T16). Two-phase: claiming the key
//! first turns a mid-flight retry into a `409` instead of a second publish.

use notify_ports::{Claim, IdempotencyStore, PortResult};
use time::OffsetDateTime;

use crate::PgStore;
use crate::mapping::fail;

/// How long a claim stays in flight before takeover: covers a slow publish, but a crashed holder
/// must not cause `409`s for the whole replay window.
const IN_FLIGHT: time::Duration = time::Duration::minutes(5);

#[async_trait::async_trait]
impl IdempotencyStore for PgStore {
    async fn claim(
        &self,
        principal_ref: &str,
        key: &str,
        now: OffsetDateTime,
    ) -> PortResult<Claim> {
        // One statement so two requests cannot both get `Fresh`; the `WHERE` reclaims lapsed reservations.
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
        .bind(now.saturating_add(IN_FLIGHT))
        .fetch_optional(self.pool())
        .await
        .map_err(|error| fail("idempotency", error))?;

        if claimed.is_some() {
            return Ok(Claim::Fresh);
        }

        // Held by someone else: finished means replay, otherwise `409`.
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
            // `409` + `Retry-After` rather than holding the request open.
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
        // `claimed_at` identifies the claim, so a late original cannot clobber its successor.
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
        // Only our unfinished claim: never delete a stored response or a taken-over claim.
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
