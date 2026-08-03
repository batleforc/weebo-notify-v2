//! Interactions and the per-user silence state (design §2.3).
//!
//! The deduplication rules are the domain's: `ack` is idempotent per (user, key),
//! `choice` is exclusive and last-write-wins. They are enforced *again* here by
//! unique indexes, and that duplication is deliberate — the domain reasons about a
//! snapshot, and two acknowledgements arriving together are a race it cannot see.

use notify_domain::{
    ActionKey, Author, Cursor, Interaction, InteractionId, InteractionRecord, NotificationId,
    Recorded, Silence, SilenceTarget, UserId,
};
use notify_ports::{Audience, EventPayload, InteractionRepository, PortResult, PreferenceEvent};
use sqlx::Row as _;
use sqlx::postgres::PgRow;
use time::OffsetDateTime;

use crate::PgStore;
use crate::mapping::{append_event, fail, from_json, parse, to_json};

/// The textual form a silence indexes under, so re-silencing the same thing
/// extends the existing silence rather than stacking a second one nobody can see
/// to lift.
fn target_reference(target: &SilenceTarget) -> String {
    // Namespaced by kind: a group key and a notification id could otherwise
    // collide, and silencing one would lift the other.
    match target {
        SilenceTarget::Group { key } => format!("group:{key}"),
        SilenceTarget::Notification { id } => format!("notification:{id}"),
    }
}

#[async_trait::async_trait]
impl InteractionRepository for PgStore {
    async fn apply(&self, outcome: &Recorded) -> PortResult<Option<Cursor>> {
        // `Unchanged` writes nothing and broadcasts nothing — that is the whole
        // point of the domain returning it rather than a bare record.
        let Some(record) = outcome.record() else {
            return Ok(None);
        };

        let mut transaction = self
            .pool()
            .begin()
            .await
            .map_err(|error| fail("interaction", error))?;

        let channel: Option<String> =
            sqlx::query_scalar("SELECT channel_id FROM notifications WHERE id = $1")
                .bind(record.notification_id.to_string())
                .fetch_optional(&mut *transaction)
                .await
                .map_err(|error| fail("interaction", error))?;

        let Some(channel) = channel else {
            return Ok(None);
        };
        let channel = parse(
            "interaction.channel",
            &channel,
            notify_domain::ChannelId::parse,
        )?;

        if let Recorded::Replaced { previous, .. } = outcome {
            // The pick it displaced goes before the new one lands, so the exclusive
            // index has room.
            sqlx::query("DELETE FROM interactions WHERE id = $1")
                .bind(previous.to_string())
                .execute(&mut *transaction)
                .await
                .map_err(|error| fail("interaction", error))?;
        }

        let kind = match &record.interaction {
            Interaction::Ack { .. } => "ack",
            Interaction::Reply { .. } => "reply",
            Interaction::Choice { .. } => "choice",
        };

        sqlx::query(
            "INSERT INTO interactions
                 (id, notification_id, author_ref, author, interaction_kind, action_key,
                  interaction, recorded_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(record.id.to_string())
        .bind(record.notification_id.to_string())
        .bind(record.author.principal.reference())
        .bind(to_json("interaction.author", &record.author)?)
        .bind(kind)
        .bind(record.interaction.key().map(ActionKey::to_string))
        .bind(to_json("interaction", &record.interaction)?)
        .bind(record.recorded_at)
        .execute(&mut *transaction)
        .await
        .map_err(|error| fail("interaction", error))?;

        let cursor = append_event(
            &mut transaction,
            &Audience::Channel { id: channel },
            &EventPayload::Interaction(record.clone()),
            record.recorded_at,
        )
        .await?;

        transaction
            .commit()
            .await
            .map_err(|error| fail("interaction", error))?;

        Ok(Some(cursor))
    }

    async fn list_for(&self, notification: NotificationId) -> PortResult<Vec<InteractionRecord>> {
        // The whole set rather than a page: this is what a service waiting on its
        // `approve` polls, and an approval that fell off page two would be a very
        // confusing bug.
        let rows = sqlx::query(
            "SELECT id, notification_id, author, interaction, recorded_at
             FROM interactions WHERE notification_id = $1 ORDER BY recorded_at, id",
        )
        .bind(notification.to_string())
        .fetch_all(self.pool())
        .await
        .map_err(|error| fail("interaction", error))?;

        rows.iter().map(hydrate).collect()
    }

    async fn silence(&self, silence: &Silence) -> PortResult<Cursor> {
        let mut transaction = self
            .pool()
            .begin()
            .await
            .map_err(|error| fail("silence", error))?;

        sqlx::query(
            "INSERT INTO silences (user_id, target_ref, target, since, until)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (user_id, target_ref) DO UPDATE
               SET since = EXCLUDED.since, until = EXCLUDED.until",
        )
        .bind(silence.user.to_string())
        .bind(target_reference(&silence.target))
        .bind(to_json("silence.target", &silence.target)?)
        .bind(silence.since)
        .bind(silence.until)
        .execute(&mut *transaction)
        .await
        .map_err(|error| fail("silence", error))?;

        // The event carries the whole preference state rather than a delta: a
        // client that missed one would otherwise apply the next to the wrong
        // baseline, and "my phone kept buzzing after I turned on do-not-disturb" is
        // not a bug anybody enjoys chasing (design §8.1).
        let silences = self.silences_for(&mut transaction, silence.user).await?;

        // Addressed to the user, never to the channel: sharing a channel with
        // somebody does not make their silences your business (scenario T26).
        let cursor = append_event(
            &mut transaction,
            &Audience::User { id: silence.user },
            &EventPayload::Preference(Box::new(PreferenceEvent {
                user: silence.user,
                silences,
            })),
            silence.since,
        )
        .await?;

        transaction
            .commit()
            .await
            .map_err(|error| fail("silence", error))?;

        Ok(cursor)
    }

    async fn active_silences(&self, user: UserId, now: OffsetDateTime) -> PortResult<Vec<Silence>> {
        let rows = sqlx::query(
            "SELECT user_id, target, since, until
             FROM silences
             WHERE user_id = $1 AND (until IS NULL OR until > $2)
             ORDER BY target_ref",
        )
        .bind(user.to_string())
        .bind(now)
        .fetch_all(self.pool())
        .await
        .map_err(|error| fail("silence", error))?;

        rows.iter().map(hydrate_silence).collect()
    }
}

impl PgStore {
    /// Every silence still in force for a user, read inside the caller's
    /// transaction so the event it goes into cannot disagree with the row that
    /// caused it.
    async fn silences_for(
        &self,
        transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        user: UserId,
    ) -> PortResult<Vec<Silence>> {
        let rows = sqlx::query(
            "SELECT user_id, target, since, until FROM silences
             WHERE user_id = $1 ORDER BY target_ref",
        )
        .bind(user.to_string())
        .fetch_all(&mut **transaction)
        .await
        .map_err(|error| fail("silence", error))?;

        rows.iter().map(hydrate_silence).collect()
    }
}

fn hydrate(row: &PgRow) -> PortResult<InteractionRecord> {
    let id: String = row.try_get("id").map_err(|e| fail("interaction", e))?;
    let notification_id: String = row
        .try_get("notification_id")
        .map_err(|e| fail("interaction", e))?;
    let author: serde_json::Value = row.try_get("author").map_err(|e| fail("interaction", e))?;
    let interaction: serde_json::Value = row
        .try_get("interaction")
        .map_err(|e| fail("interaction", e))?;

    Ok(InteractionRecord {
        id: parse("interaction.id", &id, str::parse::<InteractionId>)?,
        notification_id: parse(
            "interaction.notification_id",
            &notification_id,
            str::parse::<NotificationId>,
        )?,
        author: from_json::<Author>("interaction.author", author)?,
        interaction: from_json::<Interaction>("interaction", interaction)?,
        recorded_at: row
            .try_get("recorded_at")
            .map_err(|e| fail("interaction", e))?,
    })
}

fn hydrate_silence(row: &PgRow) -> PortResult<Silence> {
    let user: String = row.try_get("user_id").map_err(|e| fail("silence", e))?;
    let target: serde_json::Value = row.try_get("target").map_err(|e| fail("silence", e))?;

    Ok(Silence {
        user: parse("silence.user", &user, str::parse::<UserId>)?,
        target: from_json::<SilenceTarget>("silence.target", target)?,
        until: row.try_get("until").map_err(|e| fail("silence", e))?,
        since: row.try_get("since").map_err(|e| fail("silence", e))?,
    })
}
