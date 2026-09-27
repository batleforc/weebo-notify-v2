//! Do not disturb, per user (design §7.4).
//!
//! Stored as one jsonb document because it is read and written whole and never
//! queried by its parts — the reasoning is in the migration, where the schema is.
//!
//! What lives here rather than in the port is [`PgStore::announce_preference`]: the
//! single place that builds the `preference` event of §8.1. Both writers reach it —
//! setting a do-not-disturb and starting a silence — because the event carries the
//! whole preference and there is no such thing as half of it on the wire. Two call
//! sites building it separately is how one of them ends up shipping the half it
//! happens to have just written.

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

        // No row is not an absence to report: "never set one" and "set one that
        // mutes nothing" are the same answer to every caller (design §7.4).
        let Some(row) = row else {
            return Ok(Dnd::off(user));
        };

        let document: serde_json::Value = row.try_get("preference").map_err(|e| fail("dnd", e))?;
        let stored: Dnd = from_json("dnd", document)?;

        // Storage is not a trust boundary we get to skip: a row written by last
        // month's build still has to satisfy this month's invariants, and a window
        // whose timezone no longer resolves must be reported rather than silently
        // muting nothing.
        //
        // `Corrupt` and not a client error, even though the immediate cause is a
        // failed validation: we wrote this row, so the domain refusing it back is
        // our fault and nobody else's (§4, two error types on purpose).
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
        // No row means no preference means nothing could have muted them, so a
        // statement that matches nothing is the right outcome (design §7.4).
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
        // One statement, so a `defer` landing in between is either before it (and
        // was in the digest) or after it (and survives for the next one).
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
    /// Append the `preference` event of §8.1, carrying both halves of what this
    /// user has asked not to be bothered by.
    ///
    /// Read inside the caller's transaction so the event cannot disagree with the
    /// row that caused it, and addressed to the user rather than to a channel:
    /// sharing a channel with somebody does not make their preferences your
    /// business (scenario T26).
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
