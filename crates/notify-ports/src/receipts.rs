//! Delivery and read receipts (design §7.3).

use notify_domain::{Cursor, NotificationId, Receipt};

use crate::error::PortResult;

/// Who learns that a receipt was recorded (design §7.3).
///
/// The row is written either way; this is only about the event appended beside it,
/// and it exists because "we record that you read it" and "we show the others that
/// you read it" turned out to be two different questions (D27).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    /// Everybody with `subscribe` on the channel — what "vu par 3/5" is made of,
    /// and what a channel opts into with `receipts_enabled`.
    Channel,

    /// Only the reader's own connections, and nobody else's ever.
    ///
    /// What the unread badge needs on a channel that collects no receipts: reading
    /// on a phone has to drop the badge on the laptop, which is an event, while the
    /// other members must not learn that anybody read anything, which is why it
    /// cannot be the channel's.
    Reader,
}

/// Receipts.
#[async_trait::async_trait]
pub trait ReceiptRepository: Send + Sync {
    /// Record a receipt and append its event row.
    ///
    /// Idempotent per (principal, device, notification, kind): a client that retries
    /// a delivery acknowledgement, or a notification mirrored across three devices,
    /// must not inflate "seen by 3/5". `None` means nothing was broadcast — either
    /// because it was already recorded, or because a [`Visibility::Reader`] receipt
    /// belongs to a service account, which has no private audience to poke. `None`
    /// therefore never means "not recorded": the row the badge counts is written
    /// before this returns.
    ///
    /// `visibility` decides who the appended event reaches. It is a parameter and
    /// not something this port works out for itself: the answer depends on the
    /// channel's policy and on the kind of receipt, and a repository that read a
    /// policy in order to decide who may see something would be the authorisation
    /// check living in the wrong layer.
    async fn record(&self, receipt: &Receipt, visibility: Visibility)
    -> PortResult<Option<Cursor>>;

    /// Every receipt on a notification, for the "seen by 3/5" summary.
    async fn list_for(&self, notification: NotificationId) -> PortResult<Vec<Receipt>>;
}

// `has_seen(notification, principal) -> bool` used to live here, described as "the
// one question the unread badge asks". It was implemented twice, tested once, and
// **called by nobody** — which is the whole of D27, and the reason it is gone
// rather than finally wired.
//
// The badge does not ask that question. It asks how many notifications a *channel*
// holds that a principal has not read, which is one aggregate over a table and not
// N round trips over a page. Keeping a per-notification predicate around so that
// D27 could be closed "by giving it a caller" would have produced the slow version
// of the right feature, and left the fast one unwritten.
//
// See `NotificationRepository::unread_counts`, which is where the question that is
// actually asked now lives — on the port that owns visibility, because "unread"
// has to exclude deleted and expired rows exactly as a listing does.
