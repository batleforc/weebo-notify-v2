//! The outbound delivery queue and its dead letters (design §7.1bis, T17).
//! Claiming is one `UPDATE … FOR UPDATE SKIP LOCKED` statement, so failover leaders cannot both claim;
//! rows are leased, not deleted, so a killed drain's work comes back. Sink targets/secrets are not stored.

use notify_domain::{ChannelId, DeliveryId, SinkId};
use notify_ports::{DeadLetter, Event, PortError, PortResult, SinkDelivery, SinkQueue};
use sqlx::Row as _;
use time::OffsetDateTime;

use crate::PgStore;
use crate::mapping::{fail, from_json, to_json};

/// Read one queued delivery back.
fn hydrate(row: &sqlx::postgres::PgRow) -> PortResult<SinkDelivery> {
    let delivery_id: String = row
        .try_get("delivery_id")
        .map_err(|e| fail("sink.queue", e))?;
    let channel: String = row
        .try_get("channel_id")
        .map_err(|e| fail("sink.queue", e))?;
    let sink: String = row.try_get("sink").map_err(|e| fail("sink.queue", e))?;
    let event: serde_json::Value = row.try_get("event").map_err(|e| fail("sink.queue", e))?;
    let attempts: i32 = row.try_get("attempts").map_err(|e| fail("sink.queue", e))?;
    let first_failed_at: Option<OffsetDateTime> = row
        .try_get("first_failed_at")
        .map_err(|e| fail("sink.queue", e))?;

    Ok(SinkDelivery {
        delivery_id: delivery_id
            .parse()
            .map_err(|error| PortError::corrupt("sink.queue.delivery_id", error))?,
        channel: ChannelId::parse(&channel)
            .map_err(|error| PortError::corrupt("sink.queue.channel_id", error))?,
        sink: SinkId::parse(&sink).map_err(|error| PortError::corrupt("sink.queue.sink", error))?,
        event: from_json::<Event>("sink.queue.event", event)?,
        // Unreachable; clamped so a mangled counter still gets attempted.
        attempts: attempts.try_into().unwrap_or(0),
        first_failed_at,
    })
}

#[async_trait::async_trait]
impl SinkQueue for PgStore {
    async fn enqueue(
        &self,
        delivery: &SinkDelivery,
        next_attempt_at: OffsetDateTime,
    ) -> PortResult<()> {
        // Keyed on the delivery id (enqueue and reschedule alike), clearing the lease.
        sqlx::query(
            "INSERT INTO sink_queue
                 (delivery_id, channel_id, sink, event, attempts, first_failed_at, due_at,
                  leased_until)
             VALUES ($1, $2, $3, $4, $5, $6, $7, NULL)
             ON CONFLICT (delivery_id) DO UPDATE
               SET attempts        = EXCLUDED.attempts,
                   first_failed_at = EXCLUDED.first_failed_at,
                   due_at          = EXCLUDED.due_at,
                   leased_until    = NULL",
        )
        .bind(delivery.delivery_id.to_string())
        .bind(delivery.channel.as_str())
        .bind(delivery.sink.as_str())
        .bind(to_json("sink.queue.event", &delivery.event)?)
        .bind(i32::try_from(delivery.attempts).unwrap_or(i32::MAX))
        .bind(delivery.first_failed_at)
        .bind(next_attempt_at)
        .execute(self.pool())
        .await
        .map_err(|error| fail("sink.queue", error))?;

        Ok(())
    }

    async fn claim_due(
        &self,
        now: OffsetDateTime,
        limit: u32,
        lease_until: OffsetDateTime,
    ) -> PortResult<Vec<SinkDelivery>> {
        let rows = sqlx::query(
            "UPDATE sink_queue
                SET leased_until = $3
              WHERE delivery_id IN (
                    SELECT delivery_id
                      FROM sink_queue
                     WHERE due_at <= $1
                       AND (leased_until IS NULL OR leased_until <= $1)
                     ORDER BY due_at
                     LIMIT $2
                     -- Skipping what another transaction holds rather than waiting
                     -- for it: a drain that blocked on a peer's row would serialise
                     -- the whole queue behind one slow consumer.
                     FOR UPDATE SKIP LOCKED
              )
              RETURNING delivery_id, channel_id, sink, event, attempts, first_failed_at",
        )
        .bind(now)
        .bind(i64::from(limit))
        .bind(lease_until)
        .fetch_all(self.pool())
        .await
        .map_err(|error| fail("sink.queue", error))?;

        rows.iter().map(hydrate).collect()
    }

    async fn complete(&self, delivery: &DeliveryId) -> PortResult<()> {
        sqlx::query("DELETE FROM sink_queue WHERE delivery_id = $1")
            .bind(delivery.to_string())
            .execute(self.pool())
            .await
            .map_err(|error| fail("sink.queue", error))?;

        Ok(())
    }

    async fn bury(
        &self,
        delivery: &SinkDelivery,
        error: &str,
        at: OffsetDateTime,
    ) -> PortResult<()> {
        let mut transaction = self
            .pool()
            .begin()
            .await
            .map_err(|error| fail("sink.queue", error))?;

        // Both or neither, or the next drain would resend a dead letter.
        sqlx::query("DELETE FROM sink_queue WHERE delivery_id = $1")
            .bind(delivery.delivery_id.to_string())
            .execute(&mut *transaction)
            .await
            .map_err(|error| fail("sink.queue", error))?;

        sqlx::query(
            "INSERT INTO sink_dead_letters
                 (delivery_id, channel_id, sink, event, attempts, last_error, first_failed_at,
                  dead_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (delivery_id) DO UPDATE
               SET attempts   = EXCLUDED.attempts,
                   last_error = EXCLUDED.last_error,
                   dead_at    = EXCLUDED.dead_at",
        )
        .bind(delivery.delivery_id.to_string())
        .bind(delivery.channel.as_str())
        .bind(delivery.sink.as_str())
        .bind(to_json("sink.queue.event", &delivery.event)?)
        .bind(i32::try_from(delivery.attempts).unwrap_or(i32::MAX))
        .bind(error)
        // A delivery rejected outright has never "first failed" before now.
        .bind(delivery.first_failed_at.unwrap_or(at))
        .bind(at)
        .execute(&mut *transaction)
        .await
        .map_err(|error| fail("sink.queue", error))?;

        transaction
            .commit()
            .await
            .map_err(|error| fail("sink.queue", error))
    }

    async fn dead_letters(&self, channel: Option<&ChannelId>) -> PortResult<Vec<DeadLetter>> {
        let rows = sqlx::query(
            "SELECT delivery_id, channel_id, sink, attempts, last_error, first_failed_at, dead_at
               FROM sink_dead_letters
              WHERE ($1::text IS NULL OR channel_id = $1)
              ORDER BY dead_at DESC
              LIMIT 500",
        )
        .bind(channel.map(ChannelId::as_str))
        .fetch_all(self.pool())
        .await
        .map_err(|error| fail("sink.dlq", error))?;

        rows.iter()
            .map(|row| {
                let id: String = row
                    .try_get("delivery_id")
                    .map_err(|e| fail("sink.dlq", e))?;
                let channel: String = row.try_get("channel_id").map_err(|e| fail("sink.dlq", e))?;
                let sink: String = row.try_get("sink").map_err(|e| fail("sink.dlq", e))?;
                let attempts: i32 = row.try_get("attempts").map_err(|e| fail("sink.dlq", e))?;

                Ok(DeadLetter {
                    id: id
                        .parse()
                        .map_err(|error| PortError::corrupt("sink.dlq.delivery_id", error))?,
                    channel: ChannelId::parse(&channel)
                        .map_err(|error| PortError::corrupt("sink.dlq.channel_id", error))?,
                    sink: SinkId::parse(&sink)
                        .map_err(|error| PortError::corrupt("sink.dlq.sink", error))?,
                    last_error: row.try_get("last_error").map_err(|e| fail("sink.dlq", e))?,
                    attempts: attempts.try_into().unwrap_or(0),
                    first_failed_at: row
                        .try_get("first_failed_at")
                        .map_err(|e| fail("sink.dlq", e))?,
                    dead_at: row.try_get("dead_at").map_err(|e| fail("sink.dlq", e))?,
                })
            })
            .collect()
    }

    async fn requeue(&self, id: &DeliveryId, at: OffsetDateTime) -> PortResult<bool> {
        let mut transaction = self
            .pool()
            .begin()
            .await
            .map_err(|error| fail("sink.dlq", error))?;

        let row = sqlx::query(
            "DELETE FROM sink_dead_letters
              WHERE delivery_id = $1
              RETURNING delivery_id, channel_id, sink, event, attempts",
        )
        .bind(id.to_string())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|error| fail("sink.dlq", error))?;

        let Some(row) = row else {
            return Ok(false);
        };

        let event: serde_json::Value = row.try_get("event").map_err(|e| fail("sink.dlq", e))?;
        let attempts: i32 = row.try_get("attempts").map_err(|e| fail("sink.dlq", e))?;

        sqlx::query(
            "INSERT INTO sink_queue
                 (delivery_id, channel_id, sink, event, attempts, first_failed_at, due_at,
                  leased_until)
             -- `first_failed_at` deliberately NULL: a requeue is a fresh chance, not
             -- a resumption of the day this spent failing. Carrying it over would
             -- bury the delivery again on its very next failure, which is not what
             -- an admin pressing the button is asking for.
             VALUES ($1, $2, $3, $4, $5, NULL, $6, NULL)
             ON CONFLICT (delivery_id) DO UPDATE
               SET due_at = EXCLUDED.due_at, leased_until = NULL",
        )
        .bind(
            row.try_get::<String, _>("delivery_id")
                .map_err(|e| fail("sink.dlq", e))?,
        )
        .bind(
            row.try_get::<String, _>("channel_id")
                .map_err(|e| fail("sink.dlq", e))?,
        )
        .bind(
            row.try_get::<String, _>("sink")
                .map_err(|e| fail("sink.dlq", e))?,
        )
        .bind(event)
        .bind(attempts)
        .bind(at)
        .execute(&mut *transaction)
        .await
        .map_err(|error| fail("sink.dlq", error))?;

        transaction
            .commit()
            .await
            .map_err(|error| fail("sink.dlq", error))?;

        Ok(true)
    }

    async fn dead_letter_depth(&self) -> PortResult<Vec<(ChannelId, u64)>> {
        let rows = sqlx::query(
            "SELECT channel_id, count(*) AS depth FROM sink_dead_letters GROUP BY channel_id",
        )
        .fetch_all(self.pool())
        .await
        .map_err(|error| fail("sink.dlq", error))?;

        rows.iter()
            .map(|row| {
                let channel: String = row.try_get("channel_id").map_err(|e| fail("sink.dlq", e))?;
                let depth: i64 = row.try_get("depth").map_err(|e| fail("sink.dlq", e))?;

                Ok((
                    ChannelId::parse(&channel)
                        .map_err(|error| PortError::corrupt("sink.dlq.channel_id", error))?,
                    depth.try_into().unwrap_or(0),
                ))
            })
            .collect()
    }

    async fn prune_dead_letters(&self, older_than: OffsetDateTime, limit: u32) -> PortResult<u64> {
        // Bounded like every other leader-side delete, so one pass after a long
        // outage does not hold a lock over the whole table.
        let pruned = sqlx::query(
            "DELETE FROM sink_dead_letters
              WHERE delivery_id IN (
                    SELECT delivery_id FROM sink_dead_letters
                     WHERE dead_at < $1
                     ORDER BY dead_at
                     LIMIT $2
              )",
        )
        .bind(older_than)
        .bind(i64::from(limit))
        .execute(self.pool())
        .await
        .map_err(|error| fail("sink.dlq", error))?;

        Ok(pruned.rows_affected())
    }

    async fn rekey_sinks(
        &self,
        rekey: &(dyn for<'s> Fn(&'s str) -> Option<SinkId> + Sync),
    ) -> PortResult<u64> {
        // The distinct ids rather than the rows: a boot after a long outage may have
        // thousands of deliveries owed to a handful of sinks.
        let stored: Vec<String> = sqlx::query_scalar(
            "SELECT sink FROM sink_queue UNION SELECT sink FROM sink_dead_letters",
        )
        .fetch_all(self.pool())
        .await
        .map_err(|error| fail("sink.queue", error))?;

        let mut moved = 0_u64;
        for old in stored {
            let Some(new) = rekey(&old).filter(|new| new.as_str() != old) else {
                continue;
            };
            for statement in [
                "UPDATE sink_queue SET sink = $2 WHERE sink = $1",
                "UPDATE sink_dead_letters SET sink = $2 WHERE sink = $1",
            ] {
                moved += sqlx::query(statement)
                    .bind(&old)
                    .bind(new.as_str())
                    .execute(self.pool())
                    .await
                    .map_err(|error| fail("sink.queue", error))?
                    .rows_affected();
            }
        }
        Ok(moved)
    }
}
