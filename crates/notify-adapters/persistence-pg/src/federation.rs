//! Remembering that a federated ID token has been spent (design §5.3, T11).
//!
//! The whole implementation is one `INSERT ... ON CONFLICT DO NOTHING`, and that
//! is the point. "Read whether the id is known, then write it" is two statements
//! and therefore a race: two jobs presenting the same leaked token at the same
//! instant would both read "unseen" and both be minted a token. A primary key
//! cannot be raced, so the database decides, once.

use notify_ports::{PortResult, ReplayGuard};
use time::OffsetDateTime;

use crate::PgStore;
use crate::mapping::fail;

#[async_trait::async_trait]
impl ReplayGuard for PgStore {
    async fn remember(&self, jti: &str, until: OffsetDateTime) -> PortResult<bool> {
        // `RETURNING` fires only for a row that was actually inserted, so the
        // presence of a result *is* the answer. Counting affected rows would say
        // the same thing less directly and would change meaning if an
        // `ON CONFLICT DO UPDATE` were ever added here.
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
        // Nothing here is soft-deleted: an id past its token's expiry protects
        // against nothing, because a replay of that token now fails on expiry.
        let deleted = sqlx::query("DELETE FROM federation_replay WHERE expires_at <= $1")
            .bind(now)
            .execute(self.pool())
            .await
            .map_err(|error| fail("federation.replay", error))?;

        Ok(deleted.rows_affected())
    }
}
