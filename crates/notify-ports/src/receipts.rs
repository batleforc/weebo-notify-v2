//! Delivery and read receipts (design §7.3).

use notify_domain::{Cursor, NotificationId, Principal, Receipt};

use crate::error::PortResult;

/// Receipts.
#[async_trait::async_trait]
pub trait ReceiptRepository: Send + Sync {
    /// Record a receipt and append its event row.
    ///
    /// Idempotent per (principal, device, notification, kind): a client that retries
    /// a delivery acknowledgement, or a notification mirrored across three devices,
    /// must not inflate "seen by 3/5". `None` means it was already recorded and
    /// nothing was broadcast.
    async fn record(&self, receipt: &Receipt) -> PortResult<Option<Cursor>>;

    /// Every receipt on a notification, for the "seen by 3/5" summary.
    async fn list_for(&self, notification: NotificationId) -> PortResult<Vec<Receipt>>;

    /// Whether a principal has already seen a notification — the one question the
    /// unread badge asks, and not worth loading every receipt to answer.
    async fn has_seen(
        &self,
        notification: NotificationId,
        principal: &Principal,
    ) -> PortResult<bool>;
}
