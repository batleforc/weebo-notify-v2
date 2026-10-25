//! Reading the event log (ADR-0012); appends live with the writers (see [`crate::mapping`]).
//! Compaction counts over the missed window, not the page (§7.2), bounded by [`COMPACTED_SCAN`]
//! (past it the count is a lower bound). Every read stops at `events_watermark()` (ADR-0028), so a
//! cursor never steps over a row that commits later; [`PgStore::settle`] waits briefly for it first.

/// The most window revisions the `compacted` count looks at.
const COMPACTED_SCAN: i64 = 10_000;

/// How long a read waits for the watermark; past it a long transaction is holding it back and the
/// safe prefix is served (the watcher re-pokes streams once released).
const WATERMARK_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

use notify_domain::{ChannelId, Cursor, UserId};
use notify_ports::{
    Audience, Event, EventPayload, EventStore, ExpiredWhileAway, Page, PageRequest, PortError,
    PortResult, SyncPage, SyncQuery,
};
use sqlx::Row as _;
use sqlx::postgres::PgRow;
use time::OffsetDateTime;

use crate::PgStore;
use crate::mapping::{cursor, fail, from_json, parse, sequence};

/// "This revision has a later one in the caller's window" — the compaction test,
/// shared by both halves of `sync_page!`.
macro_rules! superseded {
    () => {
        "e.kind = 'revision'
         AND EXISTS (
             SELECT 1 FROM events later
             WHERE later.notification_id = e.notification_id
               AND later.seq > e.seq
               AND later.kind = 'revision'
               AND (
                     (later.audience_kind = 'channel' AND later.channel_id = ANY ($2))
                  OR (later.audience_kind = 'user' AND later.user_id = $3)
               )
         )"
    };
}

/// One page of the compacted catch-up: `$1` cursor, `$2` visible channels, `$3`
/// user, `$4` limit. `seq` starts at 1, so `COALESCE($1, 0)` is "from the start".
/// Both halves stop at the watermark (module docs); the `later` of compaction does
/// not need to — a committed later revision supersedes whether or not it is
/// served yet, and it will be.
macro_rules! sync_page {
    () => {
        concat!(
            "WITH page AS (
                 SELECT e.* FROM (SELECT DISTINCT unnest($2::text[])) AS v(channel_id)
                 CROSS JOIN LATERAL (
                     SELECT e.seq, e.audience_kind, e.channel_id, e.user_id, e.recorded_at,
                            e.kind, e.payload
                     FROM events e
                     WHERE e.audience_kind = 'channel' AND e.channel_id = v.channel_id
                       AND e.seq > COALESCE($1::bigint, 0) AND e.seq <= events_watermark()
                       AND e.kind <> 'expired_unseen'
                       AND NOT (",
            superseded!(),
            ")
                     ORDER BY e.seq
                     LIMIT $4
                 ) e
                 UNION ALL
                 (SELECT e.seq, e.audience_kind, e.channel_id, e.user_id, e.recorded_at,
                         e.kind, e.payload
                  FROM events e
                  WHERE e.audience_kind = 'user' AND e.user_id = $3
                    AND e.seq > COALESCE($1::bigint, 0) AND e.seq <= events_watermark()
                    AND e.kind <> 'expired_unseen'
                    AND NOT (",
            superseded!(),
            ")
                  ORDER BY e.seq
                  LIMIT $4)
             )
             SELECT seq, audience_kind, channel_id, user_id, recorded_at, kind, payload
             FROM page ORDER BY seq LIMIT $4"
        )
    };
}

/// A reading of the event watermark (ADR-0028).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventWatermark {
    /// The highest cursor a reader may be served; what `head` answers.
    pub watermark: Cursor,
    /// The newest committed row, served or not.
    pub newest: Cursor,
    /// When the oldest row committed past the watermark was recorded — `None`
    /// when nothing is held back.
    pub oldest_held_back: Option<OffsetDateTime>,
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
        self.settle().await?;

        // Compaction: a revision survives when no later one of its notification is visible.
        // One `LATERAL` scan per channel bounded by `LIMIT`, so cost follows the page, not the window.
        let rows = sqlx::query(sync_page!())
            .bind(after)
            .bind(&visible)
            .bind(&user_ref)
            .bind(i64::from(limit).saturating_add(1))
            .fetch_all(self.pool())
            .await
            .map_err(|error| fail("event", error))?;

        // Over at most `COMPACTED_SCAN` revisions, no `ORDER BY` (it would scan the window); any
        // subset gives a lower bound. Revisions only go to a channel, so the user audience adds none.
        let compacted: i64 = sqlx::query_scalar(
            "SELECT count(*) - count(DISTINCT notification_id)
             FROM (SELECT notification_id FROM events
                   WHERE kind = 'revision' AND channel_id = ANY ($2)
                     AND ($1::bigint IS NULL OR seq > $1) AND seq <= events_watermark()
                   LIMIT $3) bounded",
        )
        .bind(after)
        .bind(&visible)
        .bind(COMPACTED_SCAN)
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

    async fn tail(&self, after: Cursor, page: PageRequest) -> PortResult<Page<Event>> {
        let limit = page.effective_limit();
        self.settle().await?;

        // `superseded!` without its audience test: revisions only go to a channel, so a later
        // one is visible to exactly whoever could see this one.
        let rows = sqlx::query(
            "SELECT e.seq, e.audience_kind, e.channel_id, e.user_id, e.recorded_at, e.kind,
                    e.payload
             FROM events e
             WHERE e.seq > $1 AND e.seq <= events_watermark()
               AND e.kind <> 'expired_unseen'
               AND NOT (e.kind = 'revision' AND EXISTS (
                   SELECT 1 FROM events later
                   WHERE later.notification_id = e.notification_id
                     AND later.seq > e.seq
                     AND later.kind = 'revision'
               ))
             ORDER BY e.seq
             LIMIT $2",
        )
        .bind(sequence(after))
        .bind(i64::from(limit).saturating_add(1))
        .fetch_all(self.pool())
        .await
        .map_err(|error| fail("event", error))?;

        page_of(rows, limit)
    }

    async fn head(&self) -> PortResult<Cursor> {
        // The watermark, not the newest row: "now" must not pass a row still to commit (ADR-0028).
        // Derived from transaction ids, so pruning cannot move it backwards.
        let watermark: i64 = sqlx::query_scalar("SELECT events_watermark()")
            .fetch_one(self.pool())
            .await
            .map_err(|error| fail("event", error))?;

        Ok(cursor(watermark))
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
    /// Wait (up to [`WATERMARK_WAIT`]) for the watermark to cover every row committed before this call.
    async fn settle(&self) -> PortResult<()> {
        let (newest, mut watermark): (Option<i64>, i64) =
            sqlx::query_as("SELECT (SELECT max(seq) FROM events), events_watermark()")
                .fetch_one(self.pool())
                .await
                .map_err(|error| fail("event", error))?;
        // Fixed target: re-reading the newest row would chase every concurrent write.
        let Some(newest) = newest else {
            return Ok(());
        };

        let started = std::time::Instant::now();
        let mut pause = std::time::Duration::from_millis(1);
        while watermark < newest {
            if started.elapsed() >= WATERMARK_WAIT {
                tracing::debug!(
                    newest,
                    watermark,
                    "a long transaction is holding the event watermark back; serving the safe prefix"
                );
                break;
            }
            tokio::time::sleep(pause).await;
            pause = pause
                .saturating_mul(2)
                .min(std::time::Duration::from_millis(100));
            watermark = sqlx::query_scalar("SELECT events_watermark()")
                .fetch_one(self.pool())
                .await
                .map_err(|error| fail("event", error))?;
        }

        Ok(())
    }

    /// Where the event watermark stands, for the watcher and `event_watermark_lag_seconds` (ADR-0028).
    ///
    /// # Errors
    ///
    /// Fails when the database does.
    pub async fn event_watermark(&self) -> PortResult<EventWatermark> {
        // One statement, so all three come from the same snapshot.
        let (watermark, newest, oldest_held_back): (i64, Option<i64>, Option<OffsetDateTime>) =
            sqlx::query_as(
                "SELECT events_watermark(),
                        (SELECT max(seq) FROM events),
                        (SELECT recorded_at FROM events
                          WHERE seq > events_watermark() ORDER BY seq LIMIT 1)",
            )
            .fetch_one(self.pool())
            .await
            .map_err(|error| fail("event", error))?;

        Ok(EventWatermark {
            watermark: cursor(watermark),
            newest: cursor(newest.unwrap_or(0)),
            oldest_held_back,
        })
    }

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
        // The kind is a literal, not a bind: a generic plan could not match the partial index.
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT channel_id, count(*)
             FROM events
             WHERE kind = 'expired_unseen'
               AND ($1::bigint IS NULL OR seq > $1) AND seq <= events_watermark()
               AND channel_id = ANY ($2)
             GROUP BY channel_id
             ORDER BY channel_id",
        )
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
