//! Notifications and their revisions.
//!
//! The head revision is denormalised onto `notifications`: an inbox filters on
//! state, status, priority, tags and group key and orders by recent activity, and
//! making the one hot read the one that needs a join is a poor trade. The revisions
//! table stays the history.
//!
//! Every write here opens a transaction and appends its event row inside it
//! (ADR-0012). The cursor that comes back is what the publisher is told and what
//! the fan-out poke carries.

use notify_domain::{
    AttachmentId, Author, ChannelId, Content, Cursor, ExternalId, Notification, NotificationId,
    NotificationKind, Priority, Revision, RevisionId, State, Status, TombstoneReason, Ttl,
};
use notify_ports::{
    Audience, EventPayload, ExpiredNotification, InboxQuery, NotificationRepository, Page,
    PageRequest, PortResult, RevisionEvent, SearchQuery,
};
use sqlx::Row as _;
use sqlx::postgres::PgRow;
use sqlx::{Postgres, Transaction};
use time::OffsetDateTime;

use crate::PgStore;
use crate::mapping::{
    EXPIRED_UNSEEN, append_event, cursor, enum_from_text, enum_to_text, fail, from_json, parse,
    priority_rank, sequence, to_json,
};

/// Everything a [`Notification`] needs, head revision included, plus whatever the
/// caller appends.
///
/// A macro rather than a `const`, so the result is still one `&'static str`: sqlx
/// refuses a runtime-built query string, and it is right to — a `format!` here is
/// how an injection gets in even when today's arguments happen to be literals.
macro_rules! select_notification {
    ($($tail:literal),* $(,)?) => {
        concat!(
            "SELECT n.id, n.channel_id, n.external_id, n.kind, n.last_seq, n.revision_count,
                    n.created_at, n.closed_at, n.deleted_at, n.ttl, n.cursor_seq,
                    r.id AS head_id, r.seq AS head_seq, r.author AS head_author,
                    r.received_at AS head_received_at, r.state AS head_state,
                    r.status AS head_status, r.priority AS head_priority,
                    r.content AS head_content
             FROM notifications n
             JOIN revisions r ON r.notification_id = n.id AND r.seq = n.head_seq ",
            $($tail),*
        )
    };
}

/// The text the full-text index is built from.
fn searchable(content: &Content) -> String {
    let mut text = content.title.clone();
    if let Some(body) = &content.body {
        text.push(' ');
        text.push_str(body);
    }
    for tag in &content.tags {
        text.push(' ');
        text.push_str(tag);
    }
    text
}

#[async_trait::async_trait]
impl NotificationRepository for PgStore {
    async fn get(&self, id: NotificationId) -> PortResult<Option<Notification>> {
        // The only read that sees tombstoned rows, so a caller can tell "never
        // existed" from "no longer exists" — a distinction a bare 404 loses.
        let row = sqlx::query(select_notification!("WHERE n.id = $1"))
            .bind(id.to_string())
            .fetch_optional(self.pool())
            .await
            .map_err(|error| fail("notification", error))?;

        row.as_ref().map(hydrate).transpose()
    }

    async fn find_by_external_id(
        &self,
        channel: &ChannelId,
        external_id: &ExternalId,
    ) -> PortResult<Option<Notification>> {
        // Live rows only: re-running a job whose previous notification aged out
        // creates a new one rather than resurrecting a tombstone.
        let row = sqlx::query(select_notification!(
            "WHERE n.channel_id = $1 AND n.external_id = $2 AND n.deleted_at IS NULL"
        ))
        .bind(channel.as_str())
        .bind(external_id.as_str())
        .fetch_optional(self.pool())
        .await
        .map_err(|error| fail("notification", error))?;

        row.as_ref().map(hydrate).transpose()
    }

    async fn insert(&self, notification: &Notification, revision: &Revision) -> PortResult<Cursor> {
        let mut transaction = self
            .pool()
            .begin()
            .await
            .map_err(|error| fail("notification", error))?;

        // The event first, so the row can carry the cursor it landed on and the
        // inbox can order by it without a second write.
        let cursor = append_event(
            &mut transaction,
            &Audience::Channel {
                id: notification.channel.clone(),
            },
            &EventPayload::Revision(Box::new(RevisionEvent::new(notification, revision))),
            revision.received_at,
        )
        .await?;

        sqlx::query(
            "INSERT INTO notifications
                 (id, channel_id, external_id, kind, last_seq, revision_count, created_at,
                  closed_at, deleted_at, cursor_seq, ttl, expires_at, head_seq, state, status,
                  priority, priority_rank, title, body, tags, group_key, search)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NULL, $9, $10, $11, $12, $13, $14, $15,
                     $16, $17, $18, $19, $20, to_tsvector('simple', $21))",
        )
        .bind(notification.id.to_string())
        .bind(notification.channel.as_str())
        .bind(notification.external_id.as_ref().map(ExternalId::as_str))
        .bind(enum_to_text("notification.kind", &notification.kind)?)
        .bind(i32::try_from(notification.last_seq).unwrap_or(i32::MAX))
        .bind(i32::try_from(notification.revision_count).unwrap_or(i32::MAX))
        .bind(notification.created_at)
        .bind(notification.closed_at)
        .bind(sequence(cursor))
        .bind(to_json("notification.ttl", &notification.ttl)?)
        .bind(notification.expires_at())
        .bind(i32::try_from(revision.seq).unwrap_or(i32::MAX))
        .bind(enum_to_text("state", &revision.state)?)
        .bind(enum_to_text("status", &revision.status)?)
        .bind(enum_to_text("priority", &revision.priority)?)
        .bind(priority_rank(revision.priority))
        .bind(&revision.content.title)
        .bind(revision.content.body.as_deref())
        .bind(&revision.content.tags)
        .bind(revision.content.group_key.as_deref())
        .bind(searchable(&revision.content))
        .execute(&mut *transaction)
        .await
        .map_err(|error| fail("notification.external_id", error))?;

        insert_revision(&mut transaction, revision, cursor).await?;

        transaction
            .commit()
            .await
            .map_err(|error| fail("notification", error))?;

        Ok(cursor)
    }

    async fn append_revision(
        &self,
        notification: &Notification,
        revision: &Revision,
    ) -> PortResult<Cursor> {
        let mut transaction = self
            .pool()
            .begin()
            .await
            .map_err(|error| fail("notification", error))?;

        let cursor = append_event(
            &mut transaction,
            &Audience::Channel {
                id: notification.channel.clone(),
            },
            &EventPayload::Revision(Box::new(RevisionEvent::new(notification, revision))),
            revision.received_at,
        )
        .await?;

        insert_revision(&mut transaction, revision, cursor).await?;

        // The domain has already applied last-write-wins and assigned the seq; this
        // persists that decision rather than re-deciding it.
        sqlx::query(
            "UPDATE notifications
                SET last_seq = $2, revision_count = $3, closed_at = $4, ttl = $5,
                    expires_at = $6, cursor_seq = $7, head_seq = $8, state = $9, status = $10,
                    priority = $11, priority_rank = $12, title = $13, body = $14, tags = $15,
                    group_key = $16, search = to_tsvector('simple', $17)
              WHERE id = $1",
        )
        .bind(notification.id.to_string())
        .bind(i32::try_from(notification.last_seq).unwrap_or(i32::MAX))
        .bind(i32::try_from(notification.revision_count).unwrap_or(i32::MAX))
        .bind(notification.closed_at)
        .bind(to_json("notification.ttl", &notification.ttl)?)
        .bind(notification.expires_at())
        .bind(sequence(cursor))
        .bind(i32::try_from(notification.head.seq).unwrap_or(i32::MAX))
        .bind(enum_to_text("state", &notification.head.state)?)
        .bind(enum_to_text("status", &notification.head.status)?)
        .bind(enum_to_text("priority", &notification.head.priority)?)
        .bind(priority_rank(notification.head.priority))
        .bind(&notification.head.content.title)
        .bind(notification.head.content.body.as_deref())
        .bind(&notification.head.content.tags)
        .bind(notification.head.content.group_key.as_deref())
        .bind(searchable(&notification.head.content))
        .execute(&mut *transaction)
        .await
        .map_err(|error| fail("notification", error))?;

        transaction
            .commit()
            .await
            .map_err(|error| fail("notification", error))?;

        Ok(cursor)
    }

    async fn list(&self, query: &InboxQuery) -> PortResult<Page<Notification>> {
        let limit = query.page.effective_limit();
        let channels: Vec<String> = query
            .channels
            .iter()
            .map(|channel| channel.as_str().to_owned())
            .collect();

        let states = optional_enums("state", &query.states)?;
        let statuses = optional_enums("status", &query.statuses)?;

        let rows = sqlx::query(select_notification!(
            "WHERE n.channel_id = ANY ($1)
               AND n.deleted_at IS NULL
               AND (n.expires_at IS NULL OR n.expires_at > $2)
               AND ($3::text[] IS NULL OR n.state = ANY ($3))
               AND ($4::text[] IS NULL OR n.status = ANY ($4))
               AND ($5::smallint IS NULL OR n.priority_rank >= $5)
               AND ($6::text IS NULL OR n.group_key = $6)
               AND n.tags @> $7
               AND ($8::bigint IS NULL OR n.cursor_seq < $8)
             ORDER BY n.cursor_seq DESC
             LIMIT $9"
        ))
        .bind(&channels)
        .bind(OffsetDateTime::now_utc())
        .bind(states)
        .bind(statuses)
        .bind(query.min_priority.map(priority_rank))
        .bind(query.group_key.as_deref())
        .bind(&query.tags)
        .bind(query.page.after.map(sequence))
        .bind(i64::from(limit) + 1)
        .fetch_all(self.pool())
        .await
        .map_err(|error| fail("notification", error))?;

        page_of(rows, limit, |row| hydrate(&row))
    }

    async fn revisions(&self, id: NotificationId, page: PageRequest) -> PortResult<Page<Revision>> {
        let limit = page.effective_limit();
        let rows = sqlx::query(
            "SELECT id, notification_id, seq, author, received_at, state, status, priority,
                    content, cursor_seq
             FROM revisions
             WHERE notification_id = $1 AND ($2::bigint IS NULL OR cursor_seq > $2)
             -- Oldest first: a timeline is read forwards.
             ORDER BY cursor_seq
             LIMIT $3",
        )
        .bind(id.to_string())
        .bind(page.after.map(sequence))
        .bind(i64::from(limit) + 1)
        .fetch_all(self.pool())
        .await
        .map_err(|error| fail("revision", error))?;

        page_of(rows, limit, |row| hydrate_revision(&row, ""))
    }

    async fn search(&self, query: &SearchQuery) -> PortResult<Page<Notification>> {
        let limit = query.page.effective_limit();
        let channels: Vec<String> = query
            .channels
            .iter()
            .map(|channel| channel.as_str().to_owned())
            .collect();

        // Scope first, match second — the ordering that makes it hard to break
        // scenario T27 by accident.
        let rows = sqlx::query(select_notification!(
            "WHERE n.channel_id = ANY ($1)
               AND n.deleted_at IS NULL
               AND (n.expires_at IS NULL OR n.expires_at > $2)
               AND n.search @@ plainto_tsquery('simple', $3)
               AND ($4::bigint IS NULL OR n.cursor_seq < $4)
             ORDER BY n.cursor_seq DESC
             LIMIT $5"
        ))
        .bind(&channels)
        .bind(OffsetDateTime::now_utc())
        .bind(&query.text)
        .bind(query.page.after.map(sequence))
        .bind(i64::from(limit) + 1)
        .fetch_all(self.pool())
        .await
        .map_err(|error| fail("notification", error))?;

        page_of(rows, limit, |row| hydrate(&row))
    }

    async fn mark_deleted(
        &self,
        id: NotificationId,
        at: OffsetDateTime,
    ) -> PortResult<Option<Cursor>> {
        let mut transaction = self
            .pool()
            .begin()
            .await
            .map_err(|error| fail("notification", error))?;

        // Locked and re-read: deleting twice must emit one tombstone, and two
        // requests arriving together is exactly how it would emit two.
        let existing: Option<(String, Option<OffsetDateTime>)> = sqlx::query_as(
            "SELECT channel_id, deleted_at FROM notifications WHERE id = $1 FOR UPDATE",
        )
        .bind(id.to_string())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|error| fail("notification", error))?;

        let Some((channel, None)) = existing else {
            return Ok(None);
        };
        let channel = parse("notification.channel", &channel, ChannelId::parse)?;

        let cursor = append_event(
            &mut transaction,
            &Audience::Channel { id: channel },
            &EventPayload::Tombstone {
                notification_id: id,
                reason: TombstoneReason::Deleted,
            },
            at,
        )
        .await?;

        sqlx::query("UPDATE notifications SET deleted_at = $2, cursor_seq = $3 WHERE id = $1")
            .bind(id.to_string())
            .bind(at)
            .bind(sequence(cursor))
            .execute(&mut *transaction)
            .await
            .map_err(|error| fail("notification", error))?;

        transaction
            .commit()
            .await
            .map_err(|error| fail("notification", error))?;

        Ok(Some(cursor))
    }

    async fn claim_expired(
        &self,
        now: OffsetDateTime,
        limit: u32,
    ) -> PortResult<Vec<ExpiredNotification>> {
        let mut transaction = self
            .pool()
            .begin()
            .await
            .map_err(|error| fail("notification", error))?;

        // SKIP LOCKED so a second replica that briefly believes it leads claims a
        // different batch rather than blocking on this one (scenario T22).
        let due: Vec<(String, String, bool)> = sqlx::query_as(
            "SELECT n.id, n.channel_id,
                    EXISTS (
                        SELECT 1 FROM receipts r
                        WHERE r.notification_id = n.id AND r.kind IN ('delivered', 'seen')
                    ) AS was_delivered
             FROM notifications n
             WHERE n.deleted_at IS NULL AND n.expires_at IS NOT NULL AND n.expires_at <= $1
             ORDER BY n.expires_at
             LIMIT $2
             FOR UPDATE OF n SKIP LOCKED",
        )
        .bind(now)
        .bind(i64::from(limit))
        .fetch_all(&mut *transaction)
        .await
        .map_err(|error| fail("notification", error))?;

        let mut claimed = Vec::with_capacity(due.len());
        for (id, channel, was_delivered) in due {
            let notification = parse("notification.id", &id, str::parse::<NotificationId>)?;
            let channel = parse("notification.channel", &channel, ChannelId::parse)?;

            let cursor = if was_delivered {
                // Somebody has it on screen: they need the tombstone to drop it.
                append_event(
                    &mut transaction,
                    &Audience::Channel {
                        id: channel.clone(),
                    },
                    &EventPayload::Tombstone {
                        notification_id: notification,
                        reason: TombstoneReason::Expired,
                    },
                    now,
                )
                .await?
            } else {
                // Nobody ever saw it. It stays out of the backlog and becomes a
                // number instead (design §7.2bis, scenario T9).
                expired_unseen(&mut transaction, &channel, notification, now).await?
            };

            sqlx::query("UPDATE notifications SET deleted_at = $2, cursor_seq = $3 WHERE id = $1")
                .bind(&id)
                .bind(now)
                .bind(sequence(cursor))
                .execute(&mut *transaction)
                .await
                .map_err(|error| fail("notification", error))?;

            claimed.push(ExpiredNotification {
                id: notification,
                channel,
                was_delivered,
            });
        }

        transaction
            .commit()
            .await
            .map_err(|error| fail("notification", error))?;

        Ok(claimed)
    }

    async fn attachment_is_visible(
        &self,
        attachment: &AttachmentId,
        channels: &[ChannelId],
    ) -> PortResult<bool> {
        if channels.is_empty() {
            // A caller who can read nothing can be told so without a query.
            return Ok(false);
        }

        let channels: Vec<String> = channels.iter().map(|id| id.as_str().to_owned()).collect();

        // `deleted_at IS NULL` and the expiry check together: an attachment stops
        // being downloadable exactly when the last notification showing it stops
        // being readable, which is the same moment the sweep may collect the object.
        let (visible,): (bool,) = sqlx::query_as(
            "SELECT EXISTS (
                 SELECT 1
                 FROM revisions r
                 JOIN notifications n ON n.id = r.notification_id
                 WHERE r.content->'image'->>'kind' = 'attachment'
                   AND r.content->'image'->>'ref' = $1
                   AND n.channel_id = ANY($2)
                   AND n.deleted_at IS NULL
                   AND (n.expires_at IS NULL OR n.expires_at > now())
             )",
        )
        .bind(attachment.to_string())
        .bind(&channels)
        .fetch_one(self.pool())
        .await
        .map_err(|error| fail("notification", error))?;

        Ok(visible)
    }

    async fn orphaned_attachments(
        &self,
        deleted_before: OffsetDateTime,
        limit: u32,
    ) -> PortResult<Vec<AttachmentId>> {
        // `doomed` is the same window `purge` deletes, written the same way so the
        // two agree about which rows are on their way out.
        //
        // `NOT EXISTS` rather than `NOT IN`: the subquery selects a JSON extraction
        // that is NULL for every revision without an attachment image, and `NOT IN`
        // over a set containing NULL is never true — it would return nothing at all,
        // forever, and look exactly like "no orphans to collect".
        let rows: Vec<(String,)> = sqlx::query_as(
            "WITH doomed AS (
                 SELECT id FROM notifications
                 WHERE deleted_at IS NOT NULL AND deleted_at < $1
                 ORDER BY deleted_at
                 LIMIT $2
             )
             SELECT DISTINCT mine.content->'image'->>'ref' AS attachment
             FROM revisions mine
             WHERE mine.notification_id IN (SELECT id FROM doomed)
               AND mine.content->'image'->>'kind' = 'attachment'
               AND NOT EXISTS (
                   SELECT 1 FROM revisions others
                   WHERE others.content->'image'->>'kind' = 'attachment'
                     AND others.content->'image'->>'ref' = mine.content->'image'->>'ref'
                     AND others.notification_id NOT IN (SELECT id FROM doomed)
               )",
        )
        .bind(deleted_before)
        .bind(i64::from(limit))
        .fetch_all(self.pool())
        .await
        .map_err(|error| fail("notification", error))?;

        rows.into_iter()
            .map(|(id,)| parse("attachment.id", &id, str::parse::<AttachmentId>))
            .collect()
    }

    async fn purge(&self, deleted_before: OffsetDateTime, limit: u32) -> PortResult<u64> {
        let purged = sqlx::query(
            "DELETE FROM notifications
             WHERE id IN (
                 SELECT id FROM notifications
                 WHERE deleted_at IS NOT NULL AND deleted_at < $1
                 ORDER BY deleted_at
                 LIMIT $2
             )",
        )
        .bind(deleted_before)
        .bind(i64::from(limit))
        .execute(self.pool())
        .await
        .map_err(|error| fail("notification", error))?;

        Ok(purged.rows_affected())
    }
}

/// Write one revision row.
async fn insert_revision(
    transaction: &mut Transaction<'_, Postgres>,
    revision: &Revision,
    cursor: Cursor,
) -> PortResult<()> {
    sqlx::query(
        "INSERT INTO revisions
             (id, notification_id, seq, author, received_at, state, status, priority, content,
              cursor_seq)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(revision.id.to_string())
    .bind(revision.notification_id.to_string())
    .bind(i32::try_from(revision.seq).unwrap_or(i32::MAX))
    .bind(to_json("revision.author", &revision.author)?)
    .bind(revision.received_at)
    .bind(enum_to_text("state", &revision.state)?)
    .bind(enum_to_text("status", &revision.status)?)
    .bind(enum_to_text("priority", &revision.priority)?)
    .bind(to_json("revision.content", &revision.content)?)
    .bind(sequence(cursor))
    .execute(&mut **transaction)
    .await
    .map_err(|error| fail("revision.seq", error))?;

    Ok(())
}

/// Consume a sequence value for a notification that expired unseen.
///
/// It is an `events` row so the cursor stays one unbroken order, and it carries no
/// payload because there is nothing for a client to render — only to count.
async fn expired_unseen(
    transaction: &mut Transaction<'_, Postgres>,
    channel: &ChannelId,
    notification: NotificationId,
    at: OffsetDateTime,
) -> PortResult<Cursor> {
    let sequence: i64 = sqlx::query_scalar(
        "INSERT INTO events
             (audience_kind, channel_id, user_id, recorded_at, kind, notification_id, payload)
         VALUES ('channel', $1, NULL, $2, $3, $4, NULL)
         RETURNING seq",
    )
    .bind(channel.as_str())
    .bind(at)
    .bind(EXPIRED_UNSEEN)
    .bind(notification.to_string())
    .fetch_one(&mut **transaction)
    .await
    .map_err(|error| fail("event", error))?;

    Ok(cursor(sequence))
}

/// `NULL` when the caller did not filter, an array when they did.
///
/// The distinction matters in SQL: `= ANY('{}')` matches nothing, which would turn
/// "do not filter on this" into "match nothing".
fn optional_enums<T: serde::Serialize>(
    what: &'static str,
    values: &[T],
) -> PortResult<Option<Vec<String>>> {
    if values.is_empty() {
        return Ok(None);
    }

    values
        .iter()
        .map(|value| enum_to_text(what, value))
        .collect::<PortResult<Vec<_>>>()
        .map(Some)
}

/// Take the extra row off the end and turn it into the `next` cursor.
fn page_of<T>(
    mut rows: Vec<PgRow>,
    limit: u32,
    hydrate: impl Fn(PgRow) -> PortResult<T>,
) -> PortResult<Page<T>> {
    let has_more = rows.len() > limit as usize;
    rows.truncate(limit as usize);

    let next = has_more
        .then(|| rows.last().map(|row| row.try_get::<i64, _>("cursor_seq")))
        .flatten()
        .transpose()
        .map_err(|error| fail("cursor", error))?
        .map(cursor);

    let items = rows.into_iter().map(hydrate).collect::<PortResult<_>>()?;

    Ok(match next {
        Some(next) => Page::more(items, next),
        None => Page::last(items),
    })
}

/// A joined notification row.
fn hydrate(row: &PgRow) -> PortResult<Notification> {
    let id: String = row.try_get("id").map_err(|e| fail("notification", e))?;
    let channel: String = row
        .try_get("channel_id")
        .map_err(|e| fail("notification", e))?;
    let external_id: Option<String> = row
        .try_get("external_id")
        .map_err(|e| fail("notification", e))?;
    let kind: String = row.try_get("kind").map_err(|e| fail("notification", e))?;
    let last_seq: i32 = row
        .try_get("last_seq")
        .map_err(|e| fail("notification", e))?;
    let revision_count: i32 = row
        .try_get("revision_count")
        .map_err(|e| fail("notification", e))?;
    let ttl: serde_json::Value = row.try_get("ttl").map_err(|e| fail("notification", e))?;

    Ok(Notification {
        id: parse("notification.id", &id, str::parse::<NotificationId>)?,
        channel: parse("notification.channel", &channel, ChannelId::parse)?,
        external_id: external_id
            .as_deref()
            .map(|value| parse("notification.external_id", value, ExternalId::parse))
            .transpose()?,
        kind: enum_from_text::<NotificationKind>("notification.kind", &kind)?,
        head: hydrate_revision(row, "head_")?,
        last_seq: u32::try_from(last_seq).unwrap_or(0),
        revision_count: u32::try_from(revision_count).unwrap_or(0),
        created_at: row
            .try_get("created_at")
            .map_err(|e| fail("notification", e))?,
        closed_at: row
            .try_get("closed_at")
            .map_err(|e| fail("notification", e))?,
        ttl: from_json::<Ttl>("notification.ttl", ttl)?,
        deleted_at: row
            .try_get("deleted_at")
            .map_err(|e| fail("notification", e))?,
    })
}

/// A revision, either from its own table (`prefix = ""`) or from the joined head
/// columns of a notification row (`prefix = "head_"`).
fn hydrate_revision(row: &PgRow, prefix: &str) -> PortResult<Revision> {
    let column = |name: &str| format!("{prefix}{name}");

    let id: String = row
        .try_get(column("id").as_str())
        .map_err(|e| fail("revision", e))?;

    // On a notification row the head's owner *is* that row's `id`; a revision row
    // carries it in a column of its own.
    let notification_id: String = if prefix.is_empty() {
        row.try_get("notification_id")
            .map_err(|e| fail("revision", e))?
    } else {
        row.try_get("id").map_err(|e| fail("revision", e))?
    };

    let seq: i32 = row
        .try_get(column("seq").as_str())
        .map_err(|e| fail("revision", e))?;
    let author: serde_json::Value = row
        .try_get(column("author").as_str())
        .map_err(|e| fail("revision", e))?;
    let state: String = row
        .try_get(column("state").as_str())
        .map_err(|e| fail("revision", e))?;
    let status: String = row
        .try_get(column("status").as_str())
        .map_err(|e| fail("revision", e))?;
    let priority: String = row
        .try_get(column("priority").as_str())
        .map_err(|e| fail("revision", e))?;
    let content: serde_json::Value = row
        .try_get(column("content").as_str())
        .map_err(|e| fail("revision", e))?;

    Ok(Revision {
        id: parse("revision.id", &id, str::parse::<RevisionId>)?,
        notification_id: parse(
            "revision.notification_id",
            &notification_id,
            str::parse::<NotificationId>,
        )?,
        seq: u32::try_from(seq).unwrap_or(0),
        author: from_json::<Author>("revision.author", author)?,
        received_at: row
            .try_get(column("received_at").as_str())
            .map_err(|e| fail("revision", e))?,
        state: enum_from_text::<State>("state", &state)?,
        status: enum_from_text::<Status>("status", &status)?,
        priority: enum_from_text::<Priority>("priority", &priority)?,
        content: from_json::<Content>("revision.content", content)?,
    })
}
