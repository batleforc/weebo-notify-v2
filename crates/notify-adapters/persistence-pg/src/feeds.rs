//! What the feed poller knows of each feed (RFC 0002 §6.4). Every save is
//! conditional on the row's `version`, so a leader that lost its lock without
//! noticing writes nothing (decision 11).

use notify_domain::ChannelId;
use notify_ports::{FeedKey, FeedState, FeedStateStore, PendingItem, PortError, PortResult};
use sqlx::Row as _;
use time::OffsetDateTime;

use crate::PgStore;
use crate::mapping::{fail, from_json, to_json};

/// A macro, not a const: sqlx takes only `&'static str` literals for a query.
macro_rules! columns {
    () => {
        "feed_key, channel_id, seen, batch, version, etag, last_modified, failures, \
         next_poll_at, last_ok_at"
    };
}

fn hydrate(row: &sqlx::postgres::PgRow) -> PortResult<FeedState> {
    let get = |e| fail("feed_state", e);
    let channel: String = row.try_get("channel_id").map_err(get)?;
    let failures: i32 = row.try_get("failures").map_err(get)?;
    Ok(FeedState {
        key: FeedKey::from_stored(row.try_get("feed_key").map_err(get)?),
        channel: ChannelId::parse(&channel)
            .map_err(|error| PortError::corrupt("feed_state.channel_id", error))?,
        seen: row.try_get("seen").map_err(get)?,
        batch: from_json::<Vec<PendingItem>>(
            "feed_state.batch",
            row.try_get("batch").map_err(get)?,
        )?,
        version: row.try_get("version").map_err(get)?,
        etag: row.try_get("etag").map_err(get)?,
        last_modified: row.try_get("last_modified").map_err(get)?,
        failures: failures.try_into().unwrap_or(0),
        next_poll_at: row.try_get("next_poll_at").map_err(get)?,
        last_ok_at: row.try_get("last_ok_at").map_err(get)?,
    })
}

#[async_trait::async_trait]
impl FeedStateStore for PgStore {
    async fn ensure(&self, fresh: &[FeedState]) -> PortResult<()> {
        let keys: Vec<&str> = fresh.iter().map(|state| state.key.as_str()).collect();
        let channels: Vec<&str> = fresh.iter().map(|state| state.channel.as_str()).collect();
        let due: Vec<OffsetDateTime> = fresh.iter().map(|state| state.next_poll_at).collect();
        // Joined on `channels`: a feed whose channel the reconciler has not created
        // yet gets no row this pass, rather than failing the others on the key.
        sqlx::query(
            "INSERT INTO feed_state (feed_key, channel_id, next_poll_at, touched_at)
             SELECT f.feed_key, f.channel_id, f.next_poll_at, now()
               FROM unnest($1::text[], $2::text[], $3::timestamptz[])
                    AS f (feed_key, channel_id, next_poll_at)
               JOIN channels c ON c.id = f.channel_id
             ON CONFLICT (feed_key) DO NOTHING",
        )
        .bind(keys)
        .bind(channels)
        .bind(due)
        .execute(self.pool())
        .await
        .map_err(|error| fail("feed_state", error))?;
        Ok(())
    }

    async fn retain(
        &self,
        declared: &[FeedKey],
        now: OffsetDateTime,
        forget_before: OffsetDateTime,
    ) -> PortResult<u64> {
        let keys: Vec<&str> = declared.iter().map(FeedKey::as_str).collect();
        sqlx::query(
            "UPDATE feed_state SET touched_at = $2 WHERE feed_key = ANY($1) AND touched_at < $3",
        )
        .bind(&keys)
        .bind(now)
        .bind(now.saturating_sub(notify_ports::TOUCH_EVERY))
        .execute(self.pool())
        .await
        .map_err(|error| fail("feed_state", error))?;
        let deleted = sqlx::query("DELETE FROM feed_state WHERE touched_at < $1")
            .bind(forget_before)
            .execute(self.pool())
            .await
            .map_err(|error| fail("feed_state", error))?;
        Ok(deleted.rows_affected())
    }

    async fn due(
        &self,
        declared: &[FeedKey],
        now: OffsetDateTime,
        limit: u32,
    ) -> PortResult<Vec<FeedState>> {
        let keys: Vec<&str> = declared.iter().map(FeedKey::as_str).collect();
        let rows = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM feed_state WHERE next_poll_at <= $1 AND feed_key = ANY($3)",
            " ORDER BY next_poll_at LIMIT $2"
        ))
        .bind(now)
        .bind(i64::from(limit))
        .bind(keys)
        .fetch_all(self.pool())
        .await
        .map_err(|error| fail("feed_state", error))?;
        rows.iter().map(hydrate).collect()
    }

    async fn get(&self, key: &FeedKey) -> PortResult<Option<FeedState>> {
        let row = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM feed_state WHERE feed_key = $1"
        ))
        .bind(key.as_str())
        .fetch_optional(self.pool())
        .await
        .map_err(|error| fail("feed_state", error))?;
        row.as_ref().map(hydrate).transpose()
    }

    async fn save(&self, state: &FeedState) -> PortResult<bool> {
        let saved = sqlx::query(
            "UPDATE feed_state
                SET seen = $3, batch = $4, version = version + 1, etag = $5,
                    last_modified = $6, failures = $7, next_poll_at = $8, last_ok_at = $9
              WHERE feed_key = $1 AND version = $2",
        )
        .bind(state.key.as_str())
        .bind(state.version)
        .bind(state.seen.as_deref())
        .bind(to_json("feed_state.batch", &state.batch)?)
        .bind(notify_ports::feeds::clean_validator(state.etag.as_deref()))
        .bind(notify_ports::feeds::clean_validator(
            state.last_modified.as_deref(),
        ))
        .bind(i32::try_from(state.failures).unwrap_or(i32::MAX))
        .bind(state.next_poll_at)
        .bind(state.last_ok_at)
        .execute(self.pool())
        .await
        .map_err(|error| fail("feed_state", error))?;
        Ok(saved.rows_affected() == 1)
    }

    async fn failing(&self) -> PortResult<u64> {
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM feed_state WHERE failures > 0")
            .fetch_one(self.pool())
            .await
            .map_err(|error| fail("feed_state", error))?;
        Ok(count.try_into().unwrap_or(0))
    }
}
