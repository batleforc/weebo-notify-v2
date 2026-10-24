//! The fan-out outbox, and committing what a write owes inside its transaction
//! (ADR-0025).
//!
//! [`record`] runs in the write's own transaction: sink deliveries go straight into
//! `sink_queue`, the idempotent replay into `idempotency_keys`, and device push plus
//! thumbnail into `fanout_outbox`. The claim is the sink drain's: one `UPDATE …
//! SKIP LOCKED … RETURNING`, so replicas never take the same row, and a lease rather
//! than a delete, so a dead worker's rows come back.

use notify_domain::{AttachmentId, Cursor, DeviceId, Notification, NotificationId, Revision};
use notify_ports::{AfterWrite, FanoutBacklog, FanoutOutbox, PendingFanout, PortResult};
use sqlx::{Postgres, Row as _, Transaction};
use time::OffsetDateTime;

use crate::PgStore;
use crate::mapping::{cursor, fail, parse, sequence, to_json};

/// Commit `after` with the write that produced `cursor`.
pub(crate) async fn record(
    transaction: &mut Transaction<'_, Postgres>,
    notification: &Notification,
    revision: &Revision,
    at: Cursor,
    after: &AfterWrite,
) -> PortResult<()> {
    for delivery in after.deliveries(notification, revision, at) {
        // Due at once: the wait is the drain interval, not a backoff.
        sqlx::query(
            "INSERT INTO sink_queue
                 (delivery_id, channel_id, sink, event, attempts, first_failed_at, due_at,
                  leased_until)
             VALUES ($1, $2, $3, $4, 0, NULL, $5, NULL)",
        )
        .bind(delivery.delivery_id.to_string())
        .bind(delivery.channel.as_str())
        .bind(delivery.sink.as_str())
        .bind(to_json("sink.queue.event", &delivery.event)?)
        .bind(revision.received_at)
        .execute(&mut **transaction)
        .await
        .map_err(|error| fail("sink.queue", error))?;
    }

    if after.fanout {
        sqlx::query(
            "INSERT INTO fanout_outbox (cursor_seq, notification, thumbnail)
             VALUES ($1, $2, $3)",
        )
        .bind(sequence(at))
        .bind(to_json("fanout.notification", notification)?)
        .bind(after.thumbnail.map(|id| id.to_string()))
        .execute(&mut **transaction)
        .await
        .map_err(|error| fail("fanout", error))?;
    }

    if let Some(reply) = &after.reply {
        // Same guard as `IdempotencyStore::complete`: a claim taken over by
        // somebody else is theirs, and matches nothing here.
        sqlx::query(
            "UPDATE idempotency_keys
                SET response = $4, expires_at = $5
              WHERE principal_ref = $1 AND key = $2 AND claimed_at = $3 AND response IS NULL",
        )
        .bind(&reply.principal_ref)
        .bind(&reply.key)
        .bind(reply.claimed_at)
        .bind((reply.encode)(at)?)
        .bind(reply.expires_at)
        .execute(&mut **transaction)
        .await
        .map_err(|error| fail("idempotency", error))?;
    }

    Ok(())
}

#[async_trait::async_trait]
impl FanoutOutbox for PgStore {
    async fn claim_fanout(
        &self,
        now: OffsetDateTime,
        limit: u32,
        lease_until: OffsetDateTime,
        max_attempts: u32,
        too_old: OffsetDateTime,
    ) -> PortResult<Vec<PendingFanout>> {
        // Retire first, so a row that has used up its attempts — most likely one
        // whose work kills or stalls the worker every time, which therefore never
        // reports anything — stops coming back. Only unheld rows: one somebody is
        // still working on gets to finish its last attempt.
        let retired: Vec<(i64, i32, Option<String>)> = sqlx::query_as(
            "UPDATE fanout_outbox
                SET dead_at = $1,
                    last_error = COALESCE(last_error, CASE WHEN queued_at < $3
                        THEN 'too old to be worth a push' ELSE 'attempts used up' END)
              WHERE dead_at IS NULL
                AND (leased_until IS NULL OR leased_until <= $1)
                AND (attempts >= $2 OR queued_at < $3)
              RETURNING cursor_seq, attempts, last_error",
        )
        .bind(now)
        .bind(i32::try_from(max_attempts).unwrap_or(i32::MAX))
        .bind(too_old)
        .fetch_all(self.pool())
        .await
        .map_err(|error| fail("fanout", error))?;
        for (seq, attempts, last_error) in retired {
            tracing::warn!(
                cursor = seq,
                attempts,
                last_error,
                "fan-out row dead-lettered; its push will not be retried"
            );
        }

        let rows = sqlx::query(
            "UPDATE fanout_outbox
                SET leased_until = $3, attempts = attempts + 1
              WHERE cursor_seq IN (
                    SELECT cursor_seq FROM fanout_outbox o
                     WHERE dead_at IS NULL AND (leased_until IS NULL OR leased_until <= $1)
                       -- Not while another replica pushes this notification: its older
                       -- head could otherwise reach the phone after ours.
                       AND NOT EXISTS (
                           SELECT 1 FROM fanout_outbox held
                            WHERE held.notification ->> 'id' = o.notification ->> 'id'
                              AND held.cursor_seq <> o.cursor_seq
                              AND held.dead_at IS NULL AND held.leased_until > $1
                       )
                     ORDER BY cursor_seq
                     LIMIT $2
                     FOR UPDATE SKIP LOCKED
              )
              RETURNING cursor_seq, notification ->> 'id' AS notification_id, thumbnail,
                        attempts",
        )
        .bind(now)
        .bind(i64::from(limit))
        .bind(lease_until)
        .fetch_all(self.pool())
        .await
        .map_err(|error| fail("fanout", error))?;

        // Each row on its own: one that does not decode is retired on the spot —
        // it will not decode next time either — and the rest of the batch goes on.
        let mut claimed = Vec::with_capacity(rows.len());
        for row in &rows {
            let seq: i64 = row.try_get("cursor_seq").map_err(|e| fail("fanout", e))?;
            match decode(row) {
                Ok(pending) => claimed.push(pending),
                Err(error) => {
                    tracing::warn!(cursor = seq, %error, "fan-out row undecodable; dead-lettered");
                    sqlx::query(
                        "UPDATE fanout_outbox SET dead_at = $2, last_error = $3
                          WHERE cursor_seq = $1",
                    )
                    .bind(seq)
                    .bind(now)
                    .bind(error.to_string())
                    .execute(self.pool())
                    .await
                    .map_err(|error| fail("fanout", error))?;
                }
            }
        }
        Ok(claimed)
    }

    async fn complete_fanout(&self, at: Cursor) -> PortResult<()> {
        // The deliveries go with the notification's last row, not with each: a
        // later revision's row still being worked keeps its own progress. The outer
        // DELETE sees the table as it was before the CTE, hence `<> $1`.
        sqlx::query(
            "WITH done AS (
                 DELETE FROM fanout_outbox WHERE cursor_seq = $1
                 RETURNING notification ->> 'id' AS id
             )
             DELETE FROM fanout_deliveries d
              USING done
              WHERE d.notification_id = done.id
                AND NOT EXISTS (
                    SELECT 1 FROM fanout_outbox o
                     WHERE o.notification ->> 'id' = done.id AND o.cursor_seq <> $1
                )",
        )
        .bind(sequence(at))
        .execute(self.pool())
        .await
        .map_err(|error| fail("fanout", error))?;
        Ok(())
    }

    async fn extend_fanout(
        &self,
        cursors: &[Cursor],
        lease_until: OffsetDateTime,
    ) -> PortResult<()> {
        let seqs: Vec<i64> = cursors.iter().copied().map(sequence).collect();
        sqlx::query(
            "UPDATE fanout_outbox SET leased_until = $2
              WHERE cursor_seq = ANY ($1) AND dead_at IS NULL",
        )
        .bind(seqs)
        .bind(lease_until)
        .execute(self.pool())
        .await
        .map_err(|error| fail("fanout", error))?;
        Ok(())
    }

    async fn delivered_devices(
        &self,
        notification: NotificationId,
        seq: u32,
    ) -> PortResult<Vec<DeviceId>> {
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT device_id FROM fanout_deliveries WHERE notification_id = $1 AND seq = $2",
        )
        .bind(notification.to_string())
        .bind(i32::try_from(seq).unwrap_or(i32::MAX))
        .fetch_all(self.pool())
        .await
        .map_err(|error| fail("fanout", error))?;
        ids.iter()
            .map(|id| parse("fanout.device", id, str::parse::<DeviceId>))
            .collect()
    }

    async fn record_delivery(
        &self,
        notification: NotificationId,
        seq: u32,
        device: DeviceId,
    ) -> PortResult<()> {
        sqlx::query(
            "INSERT INTO fanout_deliveries (notification_id, seq, device_id)
             VALUES ($1, $2, $3)
             ON CONFLICT DO NOTHING",
        )
        .bind(notification.to_string())
        .bind(i32::try_from(seq).unwrap_or(i32::MAX))
        .bind(device.to_string())
        .execute(self.pool())
        .await
        .map_err(|error| fail("fanout", error))?;
        Ok(())
    }

    async fn retry_fanout(
        &self,
        at: Cursor,
        error: &str,
        retry_at: OffsetDateTime,
    ) -> PortResult<()> {
        sqlx::query(
            "UPDATE fanout_outbox SET leased_until = $2, last_error = $3 WHERE cursor_seq = $1",
        )
        .bind(sequence(at))
        .bind(retry_at)
        .bind(error)
        .execute(self.pool())
        .await
        .map_err(|error| fail("fanout", error))?;
        Ok(())
    }
}

/// One claimed row, or why it cannot be one.
fn decode(row: &sqlx::postgres::PgRow) -> PortResult<PendingFanout> {
    let seq: i64 = row.try_get("cursor_seq").map_err(|e| fail("fanout", e))?;
    let notification: Option<String> = row
        .try_get("notification_id")
        .map_err(|e| fail("fanout", e))?;
    let thumbnail: Option<String> = row.try_get("thumbnail").map_err(|e| fail("fanout", e))?;
    let attempts: i32 = row.try_get("attempts").map_err(|e| fail("fanout", e))?;
    Ok(PendingFanout {
        cursor: cursor(seq),
        notification: parse(
            "fanout.notification",
            notification.as_deref().unwrap_or_default(),
            str::parse::<NotificationId>,
        )?,
        thumbnail: thumbnail
            .map(|id| parse("fanout.thumbnail", &id, str::parse::<AttachmentId>))
            .transpose()?,
        attempts: u32::try_from(attempts).unwrap_or(0),
    })
}

impl PgStore {
    /// Delete up to `limit` dead-lettered rows retired before `dead_before`: an operator
    /// has had that long to read them, and the table stays small enough to count.
    ///
    /// # Errors
    ///
    /// When the database refuses the delete.
    pub async fn purge_dead_fanout(
        &self,
        dead_before: OffsetDateTime,
        limit: u32,
    ) -> PortResult<u64> {
        let deleted = sqlx::query(
            "DELETE FROM fanout_outbox
              WHERE cursor_seq IN (
                    SELECT cursor_seq FROM fanout_outbox
                     WHERE dead_at < $1
                     ORDER BY dead_at
                     LIMIT $2
              )",
        )
        .bind(dead_before)
        .bind(i64::from(limit))
        .execute(self.pool())
        .await
        .map_err(|error| fail("fanout", error))?;

        Ok(deleted.rows_affected())
    }

    /// Measure the outbox. One scan of a table that is empty when things are well.
    ///
    /// # Errors
    ///
    /// When the database cannot be read.
    pub async fn fanout_backlog(&self, now: OffsetDateTime) -> PortResult<FanoutBacklog> {
        let (pending, dead, oldest): (i64, i64, Option<OffsetDateTime>) = sqlx::query_as(
            "SELECT count(*) FILTER (WHERE dead_at IS NULL),
                    count(*) FILTER (WHERE dead_at IS NOT NULL),
                    min(queued_at) FILTER (WHERE dead_at IS NULL)
               FROM fanout_outbox",
        )
        .fetch_one(self.pool())
        .await
        .map_err(|error| fail("fanout", error))?;

        let count = |value: i64| u64::try_from(value).unwrap_or(0);
        Ok(FanoutBacklog {
            pending: count(pending),
            dead: count(dead),
            oldest_pending_age_seconds: oldest.map_or(0, |at| {
                u64::try_from((now - at).whole_seconds()).unwrap_or(0)
            }),
        })
    }
}

/// Drop what a notification still owes, in the transaction that deletes or
/// expires it: a push for something its reader can no longer open is noise, and
/// the drain's own re-read would only catch it if the row were not already
/// claimed.
pub(crate) async fn discard(
    transaction: &mut Transaction<'_, Postgres>,
    notification: NotificationId,
) -> PortResult<()> {
    sqlx::query("DELETE FROM fanout_outbox WHERE notification ->> 'id' = $1")
        .bind(notification.to_string())
        .execute(&mut **transaction)
        .await
        .map_err(|error| fail("fanout", error))?;
    Ok(())
}
