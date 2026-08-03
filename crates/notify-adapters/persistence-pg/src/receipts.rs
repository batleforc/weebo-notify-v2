//! Delivery and read receipts (design §7.3).

use notify_domain::{Cursor, DeviceId, NotificationId, Principal, Receipt, ReceiptId, ReceiptKind};
use notify_ports::{Audience, EventPayload, PortResult, ReceiptRepository};
use sqlx::Row as _;
use sqlx::postgres::PgRow;

use crate::PgStore;
use crate::mapping::{append_event, enum_from_text, enum_to_text, fail, from_json, parse, to_json};

#[async_trait::async_trait]
impl ReceiptRepository for PgStore {
    async fn record(&self, receipt: &Receipt) -> PortResult<Option<Cursor>> {
        let mut transaction = self
            .pool()
            .begin()
            .await
            .map_err(|error| fail("receipt", error))?;

        // The channel comes from the notification, and it also settles whether the
        // notification is still there at all.
        let channel: Option<String> =
            sqlx::query_scalar("SELECT channel_id FROM notifications WHERE id = $1")
                .bind(receipt.notification_id.to_string())
                .fetch_optional(&mut *transaction)
                .await
                .map_err(|error| fail("receipt", error))?;

        let Some(channel) = channel else {
            return Ok(None);
        };
        let channel = parse("receipt.channel", &channel, notify_domain::ChannelId::parse)?;

        // ON CONFLICT DO NOTHING, and the row count is the answer: a client that
        // retries a delivery acknowledgement, or a notification mirrored across
        // three devices, must not inflate "seen by 3/5".
        let inserted = sqlx::query(
            "INSERT INTO receipts
                 (id, notification_id, principal_ref, principal, is_human, device_id, kind, at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (notification_id, principal_ref, coalesce(device_id, ''), kind)
             DO NOTHING",
        )
        .bind(receipt.id.to_string())
        .bind(receipt.notification_id.to_string())
        .bind(receipt.principal.reference())
        .bind(to_json("receipt.principal", &receipt.principal)?)
        .bind(receipt.principal.is_human())
        .bind(receipt.device.map(|device| device.to_string()))
        .bind(enum_to_text("receipt.kind", &receipt.kind)?)
        .bind(receipt.at)
        .execute(&mut *transaction)
        .await
        .map_err(|error| fail("receipt", error))?;

        if inserted.rows_affected() == 0 {
            // Already recorded. Nothing written, nothing broadcast.
            return Ok(None);
        }

        let cursor = append_event(
            &mut transaction,
            &Audience::Channel { id: channel },
            &EventPayload::Receipt(receipt.clone()),
            receipt.at,
        )
        .await?;

        transaction
            .commit()
            .await
            .map_err(|error| fail("receipt", error))?;

        Ok(Some(cursor))
    }

    async fn list_for(&self, notification: NotificationId) -> PortResult<Vec<Receipt>> {
        let rows = sqlx::query(
            "SELECT id, notification_id, principal, device_id, kind, at
             FROM receipts WHERE notification_id = $1 ORDER BY at",
        )
        .bind(notification.to_string())
        .fetch_all(self.pool())
        .await
        .map_err(|error| fail("receipt", error))?;

        rows.iter().map(hydrate).collect()
    }

    async fn has_seen(
        &self,
        notification: NotificationId,
        principal: &Principal,
    ) -> PortResult<bool> {
        // The one question the unread badge asks; not worth loading every receipt
        // to answer it.
        sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1 FROM receipts
                 WHERE notification_id = $1 AND principal_ref = $2 AND kind = 'seen'
             )",
        )
        .bind(notification.to_string())
        .bind(principal.reference())
        .fetch_one(self.pool())
        .await
        .map_err(|error| fail("receipt", error))
    }
}

fn hydrate(row: &PgRow) -> PortResult<Receipt> {
    let id: String = row.try_get("id").map_err(|e| fail("receipt", e))?;
    let notification_id: String = row
        .try_get("notification_id")
        .map_err(|e| fail("receipt", e))?;
    let principal: serde_json::Value = row.try_get("principal").map_err(|e| fail("receipt", e))?;
    let device: Option<String> = row.try_get("device_id").map_err(|e| fail("receipt", e))?;
    let kind: String = row.try_get("kind").map_err(|e| fail("receipt", e))?;

    Ok(Receipt {
        id: parse("receipt.id", &id, str::parse::<ReceiptId>)?,
        notification_id: parse(
            "receipt.notification_id",
            &notification_id,
            str::parse::<NotificationId>,
        )?,
        principal: from_json::<Principal>("receipt.principal", principal)?,
        device: device
            .as_deref()
            .map(|value| parse("receipt.device", value, str::parse::<DeviceId>))
            .transpose()?,
        kind: enum_from_text::<ReceiptKind>("receipt.kind", &kind)?,
        at: row.try_get("at").map_err(|e| fail("receipt", e))?,
    })
}
