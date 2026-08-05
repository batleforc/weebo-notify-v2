//! The audit log: what was administered, by whom (design §13).
//!
//! Append-only, and a **separate stream from the technical logs** — this is the one
//! replayed after an incident, and it is no use if it is interleaved with a
//! million request lines. Mirrored to structured logs with `audit=true` so it
//! survives the database it is stored in.
//!
//! What goes in: API key creation and revocation, device and service account
//! lifecycle, membership changes — including a platform admin adding themselves to
//! a channel, which is explicitly allowed and explicitly traced (design §5.4) —
//! trust policy changes, and DLQ requeues.
//!
//! What does not: reads, publishes, receipts. Those are notifications doing their
//! job; recording them would drown the entries that matter in the ones that do not.

use notify_domain::{ChannelId, Principal};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::PortResult;
use crate::pagination::{Page, PageRequest};

/// One administrative act.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Who did it.
    pub actor: Principal,
    /// What they did, in a stable dotted vocabulary: `apikey.created`,
    /// `channel.member_added`, `service_account.disabled`, `dlq.requeued`.
    ///
    /// A closed set rather than free text, because this is what an incident review
    /// filters on.
    pub action: &'static str,
    /// What it was done to, e.g. an id. Absent when the action has no single
    /// target.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Which channel it concerned, when it concerned one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<ChannelId>,
    /// Extra context: the scopes granted, the previous value.
    ///
    /// Never secrets, never payloads. An audit log that quotes an API key is worse
    /// than no audit log, because it is a credential store nobody thinks of as one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
    /// When.
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

/// Which entries to show (design §8, `GET /admin/audit`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditQuery {
    /// Restrict to one actor.
    pub actor: Option<Principal>,
    /// Restrict to one action.
    pub action: Option<String>,
    /// Restrict to one channel.
    pub channel: Option<ChannelId>,
    /// Not before.
    pub since: Option<OffsetDateTime>,
    /// Not after.
    pub until: Option<OffsetDateTime>,
    /// Where and how much.
    pub page: PageRequest,
}

/// The audit log.
#[async_trait::async_trait]
pub trait AuditLog: Send + Sync {
    /// Record an act.
    ///
    /// Called inside the same transaction as the change it describes wherever the
    /// adapter can manage it. An audit log that can disagree with what happened is
    /// not an audit log.
    async fn record(&self, entry: &AuditEntry) -> PortResult<()>;

    /// Browse the log — platform role only, enforced by the use case (design §5.4).
    async fn query(&self, query: &AuditQuery) -> PortResult<Page<AuditEntry>>;
}
