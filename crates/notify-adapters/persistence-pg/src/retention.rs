//! Housekeeping the purge's leader does beside the purge (design §7.2bis, §13).
//!
//! Inherent on [`PgStore`] rather than on a port, like `prune_rate_limits`: these
//! are facts about this schema's bookkeeping tables, not about the domain, and no
//! use case needs to ask them.

use notify_ports::{PortResult, RetentionBacklog};
use time::OffsetDateTime;

use crate::PgStore;
use crate::mapping::fail;

/// Past this many rows a backlog count stops counting.
///
/// The gauge is there to show "falling behind", and 100 000 rows behind is
/// already that answer; counting a backlog of ten million exactly would make the
/// probe the expensive query it is meant to warn about.
const BACKLOG_COUNT_CAP: i64 = 100_000;

impl PgStore {
    /// How far the retention jobs are behind (design §13).
    ///
    /// `events_before` and `deleted_before` are the cutoffs the purge itself uses,
    /// so "pending" here means exactly "the next pass would take it". Every count
    /// is a capped range read on an index the purge already relies on.
    ///
    /// # Errors
    ///
    /// Fails when the database does.
    pub async fn retention_backlog(
        &self,
        now: OffsetDateTime,
        events_before: OffsetDateTime,
        deleted_before: OffsetDateTime,
    ) -> PortResult<RetentionBacklog> {
        let (oldest, events, expiry, purge): (Option<OffsetDateTime>, i64, i64, i64) =
            sqlx::query_as(
                "SELECT
                     (SELECT min(recorded_at) FROM events),
                     (SELECT count(*) FROM (
                          SELECT 1 FROM events WHERE recorded_at < $2 LIMIT $4) e),
                     (SELECT count(*) FROM (
                          SELECT 1 FROM notifications
                          WHERE deleted_at IS NULL AND expires_at IS NOT NULL
                            AND expires_at <= $1
                          LIMIT $4) x),
                     (SELECT count(*) FROM (
                          SELECT 1 FROM notifications
                          WHERE deleted_at IS NOT NULL AND deleted_at < $3
                          LIMIT $4) p)",
            )
            .bind(now)
            .bind(events_before)
            .bind(deleted_before)
            .bind(BACKLOG_COUNT_CAP)
            .fetch_one(self.pool())
            .await
            .map_err(|error| fail("retention", error))?;

        let rows = |count: i64| u64::try_from(count).unwrap_or(0);
        Ok(RetentionBacklog {
            oldest_event_age_seconds: oldest.map_or(0, |at| {
                u64::try_from((now - at).whole_seconds()).unwrap_or(0)
            }),
            events_prunable: rows(events),
            pending_expiry: rows(expiry),
            pending_purge: rows(purge),
        })
    }

    /// Delete devices and API keys that stopped working before `before`, and say
    /// how many of each went.
    ///
    /// A revoked device or a revoked/expired key authenticates nothing, and its
    /// row is kept only so the owner's list shows recent history. Nothing holds a
    /// foreign key to either table: a receipt names its device as plain text, and
    /// an audit entry carries the actor inline as JSON — so the audit trail of what
    /// the credential did survives its row, which is what an investigation reads.
    ///
    /// # Errors
    ///
    /// Fails when the database does.
    pub async fn prune_revoked_credentials(
        &self,
        before: OffsetDateTime,
    ) -> PortResult<(u64, u64)> {
        let devices = sqlx::query("DELETE FROM devices WHERE revoked_at < $1")
            .bind(before)
            .execute(self.pool())
            .await
            .map_err(|error| fail("device", error))?
            .rows_affected();

        let keys = sqlx::query("DELETE FROM api_keys WHERE revoked_at < $1 OR expires_at < $1")
            .bind(before)
            .execute(self.pool())
            .await
            .map_err(|error| fail("api_key", error))?
            .rows_affected();

        Ok((devices, keys))
    }
}
