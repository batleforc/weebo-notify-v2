//! The registry of signed uploads (design §7.2bis, §6.1), so never-referenced objects can be collected.

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
        // A conflict is an id collision; keep the earlier `requested_at`, which expires first.
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
        // `NOT EXISTS`, not `NOT IN`: the inner projection holds NULLs, making `NOT IN` never true.
        // Avatars are `put`, never signed, so they never appear here.
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
            // Skip the round trip on a quiet instance.
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
