//! Reading the event log (ADR-0012); appends live with the writers (see [`crate::mapping`]).
//! Compaction counts over the whole missed window, not the page (§7.2), so the scan is unbounded.

use notify_domain::{ChannelId, Cursor, UserId};
use notify_ports::{
    Audience, Event, EventPayload, EventStore, ExpiredWhileAway, Page, PortError, PortResult,
    SyncPage, SyncQuery,
};
use sqlx::Row as _;
use sqlx::postgres::PgRow;
use time::OffsetDateTime;

use crate::PgStore;
use crate::mapping::{EVENT_LOG_LOCK, EXPIRED_UNSEEN, cursor, fail, from_json, parse, sequence};

/// The events a client may see, by cursor and `?channels=`; `user` events ignore `?channels=` (T26).
macro_rules! candidates {
    ($($tail:literal),* $(,)?) => {
        concat!(
            "WITH candidate AS (
                 SELECT seq, audience_kind, channel_id, user_id, recorded_at, kind,
                        notification_id, payload
                 FROM events
                 WHERE ($1::bigint IS NULL OR seq > $1)
                   AND kind <> 'expired_unseen'
                   AND (
                         (audience_kind = 'channel' AND channel_id = ANY ($2))
                      OR (audience_kind = 'user' AND user_id = $3)
                   )
             ) ",
            $($tail),*
        )
    };
}

#[async_trait::async_trait]
impl EventStore for PgStore {
    async fn sync(&self, user: UserId, query: &SyncQuery) -> PortResult<SyncPage> {
        let visible = self
            .visible_channels(user, &query.groups, &query.channels)
            .await?;
        let after = query.after.map(sequence);
        let limit = query.page.effective_limit();
        let user_ref = user.to_string();

        // `fold = 1` keeps the newest revision per notification and every non-revision event.
        let rows = sqlx::query(candidates!(
            ", ranked AS (
                 SELECT c.*,
                        CASE WHEN c.kind = 'revision'
                             THEN row_number() OVER (
                                 PARTITION BY c.notification_id ORDER BY c.seq DESC
                             )
                             ELSE 1 END AS fold
                 FROM candidate c
             )
             SELECT seq, audience_kind, channel_id, user_id, recorded_at, kind, payload
             FROM ranked WHERE fold = 1 ORDER BY seq LIMIT $4"
        ))
        .bind(after)
        .bind(&visible)
        .bind(&user_ref)
        .bind(i64::from(limit) + 1)
        .fetch_all(self.pool())
        .await
        .map_err(|error| fail("event", error))?;

        let compacted: i64 = sqlx::query_scalar(candidates!(
            "SELECT count(*)
                    - count(DISTINCT CASE WHEN kind = 'revision' THEN notification_id END)
                    - count(*) FILTER (WHERE kind <> 'revision')
             FROM candidate"
        ))
        .bind(after)
        .bind(&visible)
        .bind(&user_ref)
        .fetch_one(self.pool())
        .await
        .map_err(|error| fail("event", error))?;

        let expired = self.expired_while_away(after, &visible).await?;
        let page = page_of(rows, limit)?;

        Ok(SyncPage {
            events: page.items,
            next: page.next,
            expired,
            compacted: u32::try_from(compacted).unwrap_or(0),
            // Filled by the use case, which reads the preference (§7.4).
            deferred_by_dnd: Vec::new(),
        })
    }

    async fn head(&self) -> PortResult<Cursor> {
        // The sequence, not `max(seq)`, so pruning cannot move the head backwards; read under the
        // shared event-log lock so "now" never passes an uncommitted row (see `lock_event_log`).
        let mut transaction = self
            .pool()
            .begin()
            .await
            .map_err(|error| fail("event", error))?;
        sqlx::query("SELECT pg_advisory_xact_lock_shared($1, $2)")
            .bind(EVENT_LOG_LOCK.0)
            .bind(EVENT_LOG_LOCK.1)
            .execute(&mut *transaction)
            .await
            .map_err(|error| fail("event", error))?;
        let sequence: Option<i64> =
            sqlx::query_scalar("SELECT pg_sequence_last_value('events_seq_seq'::regclass)")
                .fetch_one(&mut *transaction)
                .await
                .map_err(|error| fail("event", error))?;
        transaction
            .commit()
            .await
            .map_err(|error| fail("event", error))?;

        Ok(cursor(sequence.unwrap_or(0)))
    }

    async fn prune(&self, before: OffsetDateTime, limit: u32) -> PortResult<u64> {
        let pruned = sqlx::query(
            "DELETE FROM events
             WHERE seq IN (
                 SELECT seq FROM events WHERE recorded_at < $1 ORDER BY seq LIMIT $2
             )",
        )
        .bind(before)
        .bind(i64::from(limit))
        .execute(self.pool())
        .await
        .map_err(|error| fail("event", error))?;

        Ok(pruned.rows_affected())
    }
}

impl PgStore {
    /// What this user may read, narrowed by `?channels=`; narrowing can only subtract.
    async fn visible_channels(
        &self,
        user: UserId,
        groups: &[String],
        requested: &[ChannelId],
    ) -> PortResult<Vec<String>> {
        // Both doors (member and group), as the channel repository uses.
        let subscribed: Vec<String> = sqlx::query_scalar(
            "SELECT c.id FROM channels c
             WHERE EXISTS (
                 SELECT 1 FROM channel_members m
                 WHERE m.channel_id = c.id AND m.principal_ref = $1
                   AND 'subscribe' = ANY (m.scopes)
             )
             OR EXISTS (
                 SELECT 1 FROM channel_group_access g
                 WHERE g.channel_id = c.id AND g.oidc_group = ANY ($2)
                   AND 'subscribe' = ANY (g.scopes)
             )",
        )
        .bind(user.to_string())
        .bind(groups)
        .fetch_all(self.pool())
        .await
        .map_err(|error| fail("channel", error))?;

        if requested.is_empty() {
            return Ok(subscribed);
        }

        Ok(subscribed
            .into_iter()
            .filter(|channel| requested.iter().any(|wanted| wanted.as_str() == channel))
            .collect())
    }

    /// "3 notifications expired in `ci-infra` while you were away."
    async fn expired_while_away(
        &self,
        after: Option<i64>,
        visible: &[String],
    ) -> PortResult<Vec<ExpiredWhileAway>> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT channel_id, count(*)
             FROM events
             WHERE kind = $1
               AND ($2::bigint IS NULL OR seq > $2)
               AND channel_id = ANY ($3)
             GROUP BY channel_id
             ORDER BY channel_id",
        )
        .bind(EXPIRED_UNSEEN)
        .bind(after)
        .bind(visible)
        .fetch_all(self.pool())
        .await
        .map_err(|error| fail("event", error))?;

        rows.into_iter()
            .map(|(channel, count)| {
                Ok(ExpiredWhileAway {
                    channel: parse("event.channel", &channel, ChannelId::parse)?,
                    count: u32::try_from(count).unwrap_or(u32::MAX),
                })
            })
            .collect()
    }
}

/// Take the extra row off the end and turn it into the `next` cursor.
fn page_of(mut rows: Vec<PgRow>, limit: u32) -> PortResult<Page<Event>> {
    let has_more = rows.len() > limit as usize;
    rows.truncate(limit as usize);

    let next = has_more
        .then(|| rows.last().map(|row| row.try_get::<i64, _>("seq")))
        .flatten()
        .transpose()
        .map_err(|error| fail("event", error))?
        .map(cursor);

    let items = rows
        .into_iter()
        .map(|row| hydrate(&row))
        .collect::<PortResult<Vec<_>>>()?;

    Ok(match next {
        Some(next) => Page::more(items, next),
        None => Page::last(items),
    })
}

fn hydrate(row: &PgRow) -> PortResult<Event> {
    let seq: i64 = row.try_get("seq").map_err(|e| fail("event", e))?;
    let audience_kind: String = row.try_get("audience_kind").map_err(|e| fail("event", e))?;
    let payload: Option<serde_json::Value> =
        row.try_get("payload").map_err(|e| fail("event", e))?;

    let audience = if audience_kind == "user" {
        let user: String = row.try_get("user_id").map_err(|e| fail("event", e))?;
        Audience::User {
            id: parse("event.user", &user, str::parse::<UserId>)?,
        }
    } else {
        let channel: String = row.try_get("channel_id").map_err(|e| fail("event", e))?;
        Audience::Channel {
            id: parse("event.channel", &channel, ChannelId::parse)?,
        }
    };

    let Some(payload) = payload else {
        // Only `expired_unseen` rows lack a payload, and they are filtered out above.
        return Err(PortError::corrupt(
            "event.payload",
            "an event row with no payload reached the replay path",
        ));
    };

    Ok(Event {
        cursor: cursor(seq),
        audience,
        recorded_at: row.try_get("recorded_at").map_err(|e| fail("event", e))?,
        // Decided per reader on the way out (§7.4).
        muted_by_dnd: false,
        payload: from_json::<EventPayload>("event.payload", payload)?,
    })
}
