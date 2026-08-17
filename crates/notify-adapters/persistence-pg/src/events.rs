//! Reading the event log (ADR-0012).
//!
//! Appends live with the repositories that cause them — see [`crate::mapping`] —
//! because an event and the state change it describes commit together or not at
//! all. This file only ever reads.
//!
//! # Compaction is over the window, not the page
//!
//! Forty revisions of one deploy become one entry, and `compacted` reports the
//! thirty-nine that were folded (design §7.2). That count has to be computed over
//! the whole set the client missed, not over the fifty rows it happens to be
//! reading — so the candidate scan is unbounded even though the result is paged.
//! A client that has been away for a month therefore pays for a month of rows once.
//! Event retention (ADR-0012) is what keeps that bounded in practice.

use notify_domain::{ChannelId, Cursor, UserId};
use notify_ports::{
    Audience, Event, EventPayload, EventStore, ExpiredWhileAway, Page, PortError, PortResult,
    SyncPage, SyncQuery,
};
use sqlx::Row as _;
use sqlx::postgres::PgRow;
use time::OffsetDateTime;

use crate::PgStore;
use crate::mapping::{EXPIRED_UNSEEN, cursor, fail, from_json, parse, sequence};

/// The set of events a client may see, narrowed by `?channels=` and by the cursor.
///
/// A `user` event is deliberately not filtered by `?channels=`: a preference event
/// belongs to its user and to nobody else, however many channels they share, and
/// narrowing a stream to one channel does not stop it from being theirs (T26).
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

        // `fold = 1` keeps the newest revision per notification and every
        // non-revision event; what it drops is what `compacted` counts.
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

        // Everything in the window, minus one revision kept per notification, minus
        // every event that is not a revision.
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
            // Filled by the use case, which is where the preference is read: this
            // store answers "what did this user miss", not "what did they ask not to
            // be bothered by" (design §7.4).
            deferred_by_dnd: Vec::new(),
        })
    }

    async fn head(&self) -> PortResult<Cursor> {
        // The sequence rather than `max(seq)`: pruning the log must not move the
        // head backwards, or a client told to "start from now" would be handed a
        // cursor that replays what it already has.
        let sequence: Option<i64> =
            sqlx::query_scalar("SELECT pg_sequence_last_value('events_seq_seq'::regclass)")
                .fetch_one(self.pool())
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
    /// What this user may read, narrowed by an explicit `?channels=`.
    ///
    /// The narrowing can only ever subtract: a channel the caller is not a member of
    /// cannot be added back by asking for it.
    async fn visible_channels(
        &self,
        user: UserId,
        groups: &[String],
        requested: &[ChannelId],
    ) -> PortResult<Vec<String>> {
        // Both doors, same predicate the channel repository uses: a caller whose
        // access is group-based must see their backlog, not an empty one.
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
        // Only `expired_unseen` rows have no payload, and every query above filters
        // them out. Reaching here means a row this build does not understand.
        return Err(PortError::corrupt(
            "event.payload",
            "an event row with no payload reached the replay path",
        ));
    };

    Ok(Event {
        cursor: cursor(seq),
        audience,
        recorded_at: row.try_get("recorded_at").map_err(|e| fail("event", e))?,
        // Never stored, so never read back: the reader it is true for is decided
        // on the way out (§7.4, D22).
        muted_by_dnd: false,
        payload: from_json::<EventPayload>("event.payload", payload)?,
    })
}
