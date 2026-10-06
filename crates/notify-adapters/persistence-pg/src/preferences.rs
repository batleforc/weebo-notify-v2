//! Do not disturb, per user (design §7.4), stored as one jsonb document. [`PgStore::announce_preference`]
//! is the single builder of the §8.1 `preference` event, so it always carries the whole preference.

use notify_domain::{Dnd, Status, UserId};
use notify_ports::{
    Audience, DeferredTally, EventPayload, PortError, PortResult, PreferenceEvent,
    PreferenceRepository,
};
use sqlx::Row as _;
use time::OffsetDateTime;

use crate::PgStore;
use crate::mapping::{append_event, fail, from_json, to_json};
use notify_domain::Cursor;

#[async_trait::async_trait]
impl PreferenceRepository for PgStore {
    async fn dnd(&self, user: UserId) -> PortResult<Dnd> {
        let row = sqlx::query("SELECT preference FROM user_dnd WHERE user_id = $1")
            .bind(user.to_string())
            .fetch_optional(self.pool())
            .await
            .map_err(|error| fail("dnd", error))?;

        // No row and a mute-nothing row are the same answer (§7.4).
        let Some(row) = row else {
            return Ok(Dnd::off(user));
        };

        let document: serde_json::Value = row.try_get("preference").map_err(|e| fail("dnd", e))?;
        let stored: Dnd = from_json("dnd", document)?;

        // Re-validate stored rows against today's invariants; a failure is `Corrupt`, since we wrote it.
        stored
            .validated()
            .map_err(|error| PortError::corrupt("dnd", error))
    }

    async fn set_dnd(&self, dnd: &Dnd, now: OffsetDateTime) -> PortResult<Cursor> {
        let mut transaction = self.pool().begin().await.map_err(|e| fail("dnd", e))?;

        sqlx::query(
            "INSERT INTO user_dnd (user_id, preference, updated_at)
             VALUES ($1, $2, $3)
             ON CONFLICT (user_id) DO UPDATE
               SET preference = EXCLUDED.preference,
                   updated_at = EXCLUDED.updated_at,
                   -- A digest for a quiet period somebody has just redefined would
                   -- be counting something that no longer happened (design §7.4).
                   deferred_total = 0,
                   deferred_failures = 0",
        )
        .bind(dnd.user.to_string())
        .bind(to_json("dnd", dnd)?)
        .bind(now)
        .execute(&mut *transaction)
        .await
        .map_err(|error| fail("dnd", error))?;

        let cursor = self
            .announce_preference(&mut transaction, dnd.user, now)
            .await?;

        transaction
            .commit()
            .await
            .map_err(|error| fail("dnd", error))?;

        Ok(cursor)
    }

    async fn defer(&self, user: UserId, status: Status) -> PortResult<()> {
        // No row means nothing muted them, so matching nothing is correct (§7.4).
        sqlx::query(
            "UPDATE user_dnd
                SET deferred_total = deferred_total + 1,
                    deferred_failures = deferred_failures + $2
              WHERE user_id = $1",
        )
        .bind(user.to_string())
        .bind(i32::from(status == Status::Failure))
        .execute(self.pool())
        .await
        .map_err(|error| fail("dnd", error))?;

        Ok(())
    }

    async fn deferred_tallies(
        &self,
        after: Option<UserId>,
        limit: u32,
    ) -> PortResult<Vec<DeferredTally>> {
        let rows = sqlx::query(
            "SELECT preference, deferred_total, deferred_failures
               FROM user_dnd
              WHERE deferred_total > 0
                AND ($1::text IS NULL OR user_id > $1)
              ORDER BY user_id
              LIMIT $2",
        )
        .bind(after.map(|user| user.to_string()))
        .bind(i64::from(limit))
        .fetch_all(self.pool())
        .await
        .map_err(|error| fail("dnd", error))?;

        rows.iter()
            .map(|row| {
                let document: serde_json::Value =
                    row.try_get("preference").map_err(|e| fail("dnd", e))?;
                let stored: Dnd = from_json("dnd", document)?;
                let total: i32 = row.try_get("deferred_total").map_err(|e| fail("dnd", e))?;
                let failures: i32 = row
                    .try_get("deferred_failures")
                    .map_err(|e| fail("dnd", e))?;

                Ok(DeferredTally {
                    dnd: stored
                        .validated()
                        .map_err(|error| PortError::corrupt("dnd", error))?,
                    total: u32::try_from(total).unwrap_or(0),
                    failures: u32::try_from(failures).unwrap_or(0),
                })
            })
            .collect()
    }

    async fn clear_deferred(&self, user: UserId, total: u32, failures: u32) -> PortResult<()> {
        // One statement: a concurrent `defer` is either in this digest or the next.
        sqlx::query(
            "UPDATE user_dnd
                SET deferred_total = GREATEST(deferred_total - $2, 0),
                    deferred_failures = GREATEST(deferred_failures - $3, 0)
              WHERE user_id = $1",
        )
        .bind(user.to_string())
        .bind(i32::try_from(total).unwrap_or(i32::MAX))
        .bind(i32::try_from(failures).unwrap_or(i32::MAX))
        .execute(self.pool())
        .await
        .map_err(|error| fail("dnd", error))?;

        Ok(())
    }
}

impl PgStore {
    /// Append the §8.1 `preference` event inside the caller's transaction, addressed to the
    /// user rather than a channel (T26).
    pub(crate) async fn announce_preference(
        &self,
        transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        user: UserId,
        at: OffsetDateTime,
    ) -> PortResult<Cursor> {
        let row = sqlx::query("SELECT preference FROM user_dnd WHERE user_id = $1")
            .bind(user.to_string())
            .fetch_optional(&mut **transaction)
            .await
            .map_err(|error| fail("dnd", error))?;

        let dnd = match row {
            Some(row) => {
                let document: serde_json::Value =
                    row.try_get("preference").map_err(|e| fail("dnd", e))?;
                from_json("dnd", document)?
            }
            None => Dnd::off(user),
        };
        let silences = self.silences_for(transaction, user).await?;

        append_event(
            transaction,
            &Audience::User { id: user },
            &EventPayload::Preference(Box::new(PreferenceEvent {
                user,
                dnd,
                silences,
            })),
            at,
        )
        .await
    }
}
