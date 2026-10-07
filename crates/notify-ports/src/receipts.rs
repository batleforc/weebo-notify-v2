//! Delivery and read receipts (design §7.3).

use notify_domain::{Cursor, NotificationId, Receipt};

use crate::error::PortResult;

/// Who learns that a receipt was recorded (§7.3); the row is written either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    /// Everybody with `subscribe` on the channel (opt-in via `receipts_enabled`).
    Channel,

    /// Only the reader's own connections: drops the unread badge on their other
    /// devices without telling other members anything.
    Reader,
}

/// Receipts.
#[async_trait::async_trait]
pub trait ReceiptRepository: Send + Sync {
    /// Record a receipt and append its event row, idempotent per (principal, device,
    /// notification, kind). `None` means nothing was broadcast, never "not recorded".
    /// `visibility` is the caller's policy decision, not this port's.
    async fn record(&self, receipt: &Receipt, visibility: Visibility)
    -> PortResult<Option<Cursor>>;

    /// Every receipt on a notification, for the "seen by 3/5" summary.
    async fn list_for(&self, notification: NotificationId) -> PortResult<Vec<Receipt>>;
}
