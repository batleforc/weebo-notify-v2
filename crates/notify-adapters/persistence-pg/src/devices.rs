//! Devices and their sync cursors.

use notify_domain::{
    ChannelId, Cursor, Device, DeviceId, DevicePublicKey, Principal, PushTransport,
};
use notify_ports::{DeviceRepository, PortResult};
use sqlx::Row as _;
use sqlx::postgres::PgRow;
use time::OffsetDateTime;

use crate::PgStore;
use crate::mapping::{cursor, fail, from_json, parse, sequence, to_json};

const SELECT_DEVICE: &str = "
    SELECT id, owner, label, transport, public_key, cursor_seq, enrolled_at, last_seen_at,
           revoked_at
    FROM devices ";

#[async_trait::async_trait]
impl DeviceRepository for PgStore {
    async fn get(&self, id: DeviceId) -> PortResult<Option<Device>> {
        let sql = [SELECT_DEVICE, "WHERE id = $1"].concat();
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(id.to_string())
            .fetch_optional(self.pool())
            .await
            .map_err(|error| fail("device", error))?;

        row.as_ref().map(hydrate).transpose()
    }

    async fn list_for(&self, owner: &Principal) -> PortResult<Vec<Device>> {
        // Revoked devices included: a device list shows what was revoked and when.
        // Delivery filters them out itself.
        let sql = [SELECT_DEVICE, "WHERE owner_ref = $1 ORDER BY enrolled_at"].concat();
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(owner.reference())
            .fetch_all(self.pool())
            .await
            .map_err(|error| fail("device", error))?;

        rows.iter().map(hydrate).collect()
    }

    async fn upsert(&self, device: &Device) -> PortResult<()> {
        sqlx::query(
            "INSERT INTO devices
                 (id, owner_ref, owner, label, transport, public_key, cursor_seq, enrolled_at,
                  last_seen_at, revoked_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             ON CONFLICT (id) DO UPDATE
               SET owner_ref    = EXCLUDED.owner_ref,
                   owner        = EXCLUDED.owner,
                   label        = EXCLUDED.label,
                   transport    = EXCLUDED.transport,
                   public_key   = EXCLUDED.public_key,
                   cursor_seq   = EXCLUDED.cursor_seq,
                   last_seen_at = EXCLUDED.last_seen_at,
                   revoked_at   = EXCLUDED.revoked_at",
        )
        .bind(device.id.to_string())
        .bind(device.owner.reference())
        .bind(to_json("device.owner", &device.owner)?)
        .bind(&device.label)
        .bind(to_json("device.transport", &device.transport)?)
        .bind(
            device
                .public_key
                .as_ref()
                .map(|key| key.as_bytes().to_vec()),
        )
        .bind(device.cursor.map(sequence))
        .bind(device.enrolled_at)
        .bind(device.last_seen_at)
        .bind(device.revoked_at)
        .execute(self.pool())
        .await
        .map_err(|error| fail("device", error))
        .map(|_| ())
    }

    async fn revoke(&self, id: DeviceId, at: OffsetDateTime) -> PortResult<bool> {
        // The subscription, the cursor and the public key all go together
        // (design §8, §15, scenario T18). Leaving the key behind would leave a
        // sealed box addressable to a device nobody controls any more, and leaving
        // the endpoint behind would keep pushing to it.
        let revoked = sqlx::query(
            "UPDATE devices
                SET revoked_at = $2,
                    transport  = $3,
                    public_key = NULL,
                    cursor_seq = NULL
              WHERE id = $1 AND revoked_at IS NULL",
        )
        .bind(id.to_string())
        .bind(at)
        .bind(to_json("device.transport", &PushTransport::Stream)?)
        .execute(self.pool())
        .await
        .map_err(|error| fail("device", error))?;

        Ok(revoked.rows_affected() > 0)
    }

    async fn advance_cursor(
        &self,
        id: DeviceId,
        position: Cursor,
        at: OffsetDateTime,
    ) -> PortResult<()> {
        // Monotonic in the WHERE clause, not in a read-then-write: two tabs of the
        // same client acknowledging different positions must not let the older one
        // win and re-deliver what was already seen.
        sqlx::query(
            "UPDATE devices
                SET cursor_seq = $2, last_seen_at = $3
              WHERE id = $1 AND (cursor_seq IS NULL OR cursor_seq < $2)",
        )
        .bind(id.to_string())
        .bind(sequence(position))
        .bind(at)
        .execute(self.pool())
        .await
        .map_err(|error| fail("device", error))
        .map(|_| ())
    }

    async fn delivery_targets(&self, channel: &ChannelId) -> PortResult<Vec<Device>> {
        // "Who could be reached", not "who should be": silenced and do-not-disturb
        // recipients are the use case's business, not this port's (design §7.4).
        //
        // Two doors, same as `scoped_by_access!` in `channels.rs`: an individual
        // binding in `channel_members`, or a group binding in `channel_group_access`
        // resolved against the last groups `remember_groups` saw for this owner —
        // the snapshot `principal_group_snapshots` exists for.
        let sql = [
            SELECT_DEVICE,
            "WHERE revoked_at IS NULL
               AND (
                   owner_ref IN (
                       SELECT principal_ref FROM channel_members
                       WHERE channel_id = $1 AND 'subscribe' = ANY (scopes)
                   )
                   OR owner_ref IN (
                       SELECT s.principal_ref
                       FROM principal_group_snapshots s
                       JOIN channel_group_access g ON g.channel_id = $1
                       WHERE g.oidc_group = ANY (s.groups) AND 'subscribe' = ANY (g.scopes)
                   )
               )
             ORDER BY enrolled_at",
        ]
        .concat();

        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(channel.as_str())
            .fetch_all(self.pool())
            .await
            .map_err(|error| fail("device", error))?;

        rows.iter().map(hydrate).collect()
    }

    async fn remember_groups(
        &self,
        principal: &Principal,
        groups: &[String],
        at: OffsetDateTime,
    ) -> PortResult<()> {
        sqlx::query(
            "INSERT INTO principal_group_snapshots (principal_ref, groups, observed_at)
             VALUES ($1, $2, $3)
             ON CONFLICT (principal_ref) DO UPDATE
               SET groups = EXCLUDED.groups, observed_at = EXCLUDED.observed_at",
        )
        .bind(principal.reference())
        .bind(groups)
        .bind(at)
        .execute(self.pool())
        .await
        .map_err(|error| fail("device", error))?;
        Ok(())
    }
}

fn hydrate(row: &PgRow) -> PortResult<Device> {
    let id: String = row.try_get("id").map_err(|e| fail("device", e))?;
    let owner: serde_json::Value = row.try_get("owner").map_err(|e| fail("device", e))?;
    let transport: serde_json::Value = row.try_get("transport").map_err(|e| fail("device", e))?;
    let public_key: Option<Vec<u8>> = row.try_get("public_key").map_err(|e| fail("device", e))?;
    let cursor_seq: Option<i64> = row.try_get("cursor_seq").map_err(|e| fail("device", e))?;

    let public_key = public_key
        .map(|bytes| {
            <[u8; 32]>::try_from(bytes.as_slice())
                .map(DevicePublicKey::from_bytes)
                .map_err(|_| {
                    notify_ports::PortError::corrupt(
                        "device.public_key",
                        format!("expected 32 bytes, found {}", bytes.len()),
                    )
                })
        })
        .transpose()?;

    Ok(Device {
        id: parse("device.id", &id, str::parse::<DeviceId>)?,
        owner: from_json::<Principal>("device.owner", owner)?,
        label: row.try_get("label").map_err(|e| fail("device", e))?,
        transport: from_json::<PushTransport>("device.transport", transport)?,
        public_key,
        cursor: cursor_seq.map(cursor),
        enrolled_at: row.try_get("enrolled_at").map_err(|e| fail("device", e))?,
        last_seen_at: row.try_get("last_seen_at").map_err(|e| fail("device", e))?,
        revoked_at: row.try_get("revoked_at").map_err(|e| fail("device", e))?,
    })
}
