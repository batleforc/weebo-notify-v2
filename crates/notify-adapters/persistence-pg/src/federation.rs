//! Remembering spent federated ID tokens (design §5.3, T11): one `INSERT ... ON CONFLICT DO NOTHING`,
//! so the primary key, not a read-then-write race, decides.

use notify_ports::{PortResult, ReplayGuard};
use time::OffsetDateTime;

use crate::PgStore;
use crate::mapping::fail;

#[async_trait::async_trait]
impl ReplayGuard for PgStore {
    async fn remember(&self, jti: &str, until: OffsetDateTime) -> PortResult<bool> {
        // `RETURNING` fires only for an inserted row, so a result *is* the answer.
        let inserted: Option<(String,)> = sqlx::query_as(
            "INSERT INTO federation_replay (jti, expires_at)
             VALUES ($1, $2)
             ON CONFLICT (jti) DO NOTHING
             RETURNING jti",
        )
        .bind(jti)
        .bind(until)
        .fetch_optional(self.pool())
        .await
        .map_err(|error| fail("federation.replay", error))?;

        Ok(inserted.is_some())
    }

    async fn prune(&self, now: OffsetDateTime) -> PortResult<u64> {
        // An id past its token's expiry protects nothing: the replay fails on expiry.
        let deleted = sqlx::query("DELETE FROM federation_replay WHERE expires_at <= $1")
            .bind(now)
            .execute(self.pool())
            .await
            .map_err(|error| fail("federation.replay", error))?;

        Ok(deleted.rows_affected())
    }
}
