//! The registry of signed uploads (design §7.2bis, §6.1, D25).
//!
//! Not the bucket — that is `notify-attachments-s3`. This is the database
//! remembering which ids we handed out, so an upload that was never referenced by
//! any notification can still be found and collected. Without it, such an object
//! is invisible: nothing in this schema has ever seen its id.

use notify_domain::{AttachmentId, ChannelId};
use notify_ports::{AttachmentRegistry, PortResult};
use time::OffsetDateTime;

use crate::PgStore;
use crate::mapping::{fail, parse};

#[async_trait::async_trait]
impl AttachmentRegistry for PgStore {
    async fn record(
        &self,
        id: AttachmentId,
        channel: &ChannelId,
        at: OffsetDateTime,
    ) -> PortResult<()> {
        // `DO NOTHING` rather than an upsert: ids come from a generator, so a
        // conflict is not a re-signature but the astronomically unlikely collision
        // — and if it ever happened, the earlier `requested_at` is the safer one to
        // keep, since it is the one that expires first.
        sqlx::query(
            "INSERT INTO attachment_presigns (id, channel_id, requested_at)
             VALUES ($1, $2, $3)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(id.to_string())
        .bind(channel.as_str())
        .bind(at)
        .execute(self.pool())
        .await
        .map_err(|error| fail("attachment_presign", error))?;

        Ok(())
    }

    async fn unreferenced(
        &self,
        older_than: OffsetDateTime,
        limit: u32,
    ) -> PortResult<Vec<AttachmentId>> {
        // `NOT EXISTS` and not `NOT IN`, for the reason the sibling query in
        // `notifications.rs` spells out: the inner projection is NULL for every
        // revision without an attachment image, and `NOT IN` over a set holding a
        // NULL is never true — this would return nothing, for ever, and look
        // exactly like a bucket with nothing to collect.
        //
        // Avatars never appear here: they are written by `put` rather than signed,
        // so no row is ever recorded for one.
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT p.id FROM attachment_presigns p
             WHERE p.requested_at < $1
               AND NOT EXISTS (
                   SELECT 1 FROM revisions r
                   WHERE r.content->'image'->>'kind' = 'attachment'
                     AND r.content->'image'->>'ref' = p.id
               )
             ORDER BY p.requested_at
             LIMIT $2",
        )
        .bind(older_than)
        .bind(i64::from(limit))
        .fetch_all(self.pool())
        .await
        .map_err(|error| fail("attachment_presign", error))?;

        rows.into_iter()
            .map(|(id,)| parse("attachment.id", &id, str::parse::<AttachmentId>))
            .collect()
    }

    async fn forget(&self, ids: &[AttachmentId]) -> PortResult<u64> {
        if ids.is_empty() {
            // `= ANY('{}')` is valid and matches nothing, but a round trip to learn
            // that is a round trip on every tick of a quiet instance.
            return Ok(0);
        }

        let ids: Vec<String> = ids.iter().map(ToString::to_string).collect();
        let deleted = sqlx::query("DELETE FROM attachment_presigns WHERE id = ANY($1)")
            .bind(&ids)
            .execute(self.pool())
            .await
            .map_err(|error| fail("attachment_presign", error))?;

        Ok(deleted.rows_affected())
    }
}
