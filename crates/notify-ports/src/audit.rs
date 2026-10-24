//! The append-only audit log of administrative actions (§13), separate from
//! technical logs and mirrored to them with `audit=true`. Reads, publishes and receipts are not audited.

use notify_domain::{ChannelId, Principal};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::PortResult;
use crate::pagination::{Page, PageRequest};

/// Who performed an audited action: an authenticated principal, or the GitOps configuration.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AuditActor {
    /// The configuration file, applied by the reconciler. First so `untagged` tries it first.
    Configuration(Configured),

    /// Somebody or something that authenticated (design §5).
    Principal(Principal),
}

/// The single-valued tag behind [`AuditActor::Configuration`] (an enum so it serialises as `{"kind":"configuration"}`).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Configured {
    /// Applied from GitOps configuration, not a request (§11).
    Configuration,
}

impl AuditActor {
    /// The reconciler, as the log names it.
    #[must_use]
    pub const fn configuration() -> Self {
        Self::Configuration(Configured::Configuration)
    }

    /// The stable string the log is indexed and filtered on; cannot collide with a
    /// principal reference.
    #[must_use]
    pub fn reference(&self) -> String {
        match self {
            Self::Configuration(_) => "configuration".to_owned(),
            Self::Principal(principal) => principal.reference(),
        }
    }
}

impl From<Principal> for AuditActor {
    fn from(principal: Principal) -> Self {
        Self::Principal(principal)
    }
}

/// One administrative action from the closed vocabulary in [`actions`] (§13).
/// A type, not a string, so every emitted value can be read back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AuditAction(&'static str);

impl AuditAction {
    /// The wire form — what is stored, logged and filtered on.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }

    /// Read one back, refusing anything outside the vocabulary. Adapters must use this,
    /// never a list of their own.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        AUDIT_ACTIONS.iter().copied().find(|known| known.0 == value)
    }
}

impl core::fmt::Display for AuditAction {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.0)
    }
}

impl PartialEq<str> for AuditAction {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl Serialize for AuditAction {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0)
    }
}

impl<'de> Deserialize<'de> for AuditAction {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "`{value}` is not in the audit vocabulary of this build"
            ))
        })
    }
}

/// Every administrative action this build records (design §13).
pub mod actions {
    use super::AuditAction;

    /// An API key was minted.
    pub const APIKEY_CREATED: AuditAction = AuditAction("apikey.created");
    /// An API key was revoked, which takes effect immediately.
    pub const APIKEY_REVOKED: AuditAction = AuditAction("apikey.revoked");
    /// A platform-admin set (or cleared) a key's own publish ceiling (§8.2).
    pub const APIKEY_LIMIT_SET: AuditAction = AuditAction("apikey.limit_set");
    /// A channel was created through the API.
    pub const CHANNEL_CREATED: AuditAction = AuditAction("channel.created");
    /// A channel was deleted through the API.
    pub const CHANNEL_DELETED: AuditAction = AuditAction("channel.deleted");
    /// Somebody was added to a channel, or their scopes changed.
    pub const CHANNEL_MEMBER_ADDED: AuditAction = AuditAction("channel.member_added");
    /// Somebody left a channel themselves (no `manage` required).
    pub const CHANNEL_MEMBER_LEFT: AuditAction = AuditAction("channel.member_left");
    /// Somebody was taken out of a channel by a manager.
    pub const CHANNEL_MEMBER_REMOVED: AuditAction = AuditAction("channel.member_removed");
    /// A channel's retention or receipt policy was edited (`PATCH /channels/{id}`).
    pub const CHANNEL_POLICY_UPDATED: AuditAction = AuditAction("channel.policy_updated");
    /// A device was enrolled.
    pub const DEVICE_ENROLLED: AuditAction = AuditAction("device.enrolled");
    /// A device was revoked, taking its subscription, cursor and key with it.
    pub const DEVICE_REVOKED: AuditAction = AuditAction("device.revoked");
    /// A dead letter was put back on the queue (design §7.1bis).
    pub const DLQ_REQUEUED: AuditAction = AuditAction("dlq.requeued");
    /// The platform role added itself to a channel (§5.4, T30).
    pub const PLATFORM_CHANNEL_SELF_JOIN: AuditAction = AuditAction("platform.channel.self_join");
    /// A service account was created through the API.
    pub const SERVICE_ACCOUNT_CREATED: AuditAction = AuditAction("service_account.created");
    /// A service account was disabled, taking every key it owns with it.
    pub const SERVICE_ACCOUNT_DISABLED: AuditAction = AuditAction("service_account.disabled");

    // --- What GitOps changed (§11, §13): one action per change, nothing when steady.

    /// A `[[trust_policy]]` appeared — a forge may now publish here (§5.3).
    pub const TRUST_POLICY_ADDED: AuditAction = AuditAction("trust_policy.added");
    /// A `[[trust_policy]]` was edited: its issuer, channels or subjects moved.
    pub const TRUST_POLICY_CHANGED: AuditAction = AuditAction("trust_policy.changed");
    /// A `[[trust_policy]]` was withdrawn — that forge can no longer publish.
    pub const TRUST_POLICY_REMOVED: AuditAction = AuditAction("trust_policy.removed");

    /// A `[[channels.managed]]` block created a channel.
    pub const MANAGED_CHANNEL_ADDED: AuditAction = AuditAction("managed.channel.added");
    /// A managed channel's declaration changed — access, TTL, limits.
    pub const MANAGED_CHANNEL_CHANGED: AuditAction = AuditAction("managed.channel.changed");
    /// A managed channel left the configuration and was archived (never deleted).
    pub const MANAGED_CHANNEL_REMOVED: AuditAction = AuditAction("managed.channel.removed");

    /// A `[[service_accounts]]` block created an account.
    pub const MANAGED_ACCOUNT_ADDED: AuditAction = AuditAction("managed.service_account.added");
    /// A managed service account's declaration changed.
    pub const MANAGED_ACCOUNT_CHANGED: AuditAction = AuditAction("managed.service_account.changed");
    /// A managed service account left the configuration and was disabled with its keys.
    pub const MANAGED_ACCOUNT_REMOVED: AuditAction = AuditAction("managed.service_account.removed");

    /// Secrets were redacted from entries already in this log (ADR-0026). Written
    /// by a migration, never by code: its `detail` says how many rows and why, so
    /// the one sanctioned edit to the log is itself in the log.
    pub const AUDIT_REDACTED: AuditAction = AuditAction("audit.redacted");

    /// Entries past `[retention] audit` were deleted (ADR-0027). Written by the
    /// purge function itself, in the same transaction as the delete, with how many
    /// and the cutoff — so a call nobody configured still leaves its mark.
    pub const AUDIT_PURGED: AuditAction = AuditAction("audit.purged");
}

/// The closed vocabulary: every value [`AuditAction::parse`] accepts.
pub const AUDIT_ACTIONS: &[AuditAction] = &[
    actions::APIKEY_CREATED,
    actions::APIKEY_REVOKED,
    actions::APIKEY_LIMIT_SET,
    actions::CHANNEL_CREATED,
    actions::CHANNEL_DELETED,
    actions::CHANNEL_MEMBER_ADDED,
    actions::CHANNEL_MEMBER_LEFT,
    actions::CHANNEL_MEMBER_REMOVED,
    actions::CHANNEL_POLICY_UPDATED,
    actions::DEVICE_ENROLLED,
    actions::DEVICE_REVOKED,
    actions::DLQ_REQUEUED,
    actions::PLATFORM_CHANNEL_SELF_JOIN,
    actions::SERVICE_ACCOUNT_CREATED,
    actions::SERVICE_ACCOUNT_DISABLED,
    actions::TRUST_POLICY_ADDED,
    actions::TRUST_POLICY_CHANGED,
    actions::TRUST_POLICY_REMOVED,
    actions::MANAGED_CHANNEL_ADDED,
    actions::MANAGED_CHANNEL_CHANGED,
    actions::MANAGED_CHANNEL_REMOVED,
    actions::MANAGED_ACCOUNT_ADDED,
    actions::MANAGED_ACCOUNT_CHANGED,
    actions::MANAGED_ACCOUNT_REMOVED,
    actions::AUDIT_REDACTED,
    actions::AUDIT_PURGED,
];

/// The youngest an audit entry may be and still be purged (ADR-0027).
///
/// Mirrors the floor hard-coded in `wnk_audit_purge` and in the table's trigger:
/// configuration refuses anything shorter so an operator hears it at boot rather
/// than from a purge that fails every pass. Changing it here changes nothing in
/// the database, which is the point — the SQL floor is what an attacker meets.
pub const AUDIT_RETENTION_FLOOR: time::Duration = time::Duration::days(90);

#[cfg(test)]
mod action_tests {
    use super::*;

    #[test]
    fn every_action_parses_back_to_itself() {
        // Every emitted value must be one the reader accepts.
        for action in AUDIT_ACTIONS {
            assert_eq!(
                AuditAction::parse(action.as_str()),
                Some(*action),
                "{action} does not survive a round trip"
            );
        }
    }

    #[test]
    fn an_action_outside_the_vocabulary_is_refused() {
        assert_eq!(AuditAction::parse("apikey.exfiltrated"), None);
    }

    #[test]
    fn the_vocabulary_has_no_duplicates() {
        let mut seen: Vec<&str> = AUDIT_ACTIONS.iter().map(|a| a.as_str()).collect();
        seen.sort_unstable();
        seen.dedup();

        assert_eq!(seen.len(), AUDIT_ACTIONS.len());
    }

    #[test]
    fn an_action_serialises_as_its_wire_string_and_reads_back() {
        // Stored rows and `GET /admin/audit` share this shape.
        let json = serde_json::to_string(&actions::DLQ_REQUEUED).unwrap();
        assert_eq!(json, "\"dlq.requeued\"");
        assert_eq!(
            serde_json::from_str::<AuditAction>(&json).unwrap(),
            actions::DLQ_REQUEUED
        );
        assert!(serde_json::from_str::<AuditAction>("\"nope\"").is_err());
    }
}

/// One administrative act.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Who did it — or [`AuditActor::configuration`] where a file did (§11).
    pub actor: AuditActor,
    /// What they did — one of the constants in [`actions`].
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub action: AuditAction,
    /// What it was done to, e.g. an id. Absent when the action has no single target.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Which channel it concerned, when it concerned one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<ChannelId>,
    /// Extra context such as the scopes granted or the previous value; never secrets or payloads.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
    /// When.
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

/// Which entries to show (design §8, `GET /admin/audit`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditQuery {
    /// Restrict to one actor, the configuration included.
    pub actor: Option<AuditActor>,
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
    /// Record an act, in the same transaction as the change where the adapter can.
    async fn record(&self, entry: &AuditEntry) -> PortResult<()>;

    /// Browse the log — platform role only, enforced by the use case (design §5.4).
    async fn query(&self, query: &AuditQuery) -> PortResult<Page<AuditEntry>>;

    /// Delete up to `batch` entries recorded before `cutoff`, and say how many went
    /// (ADR-0027, `[retention] audit`).
    ///
    /// The one deletion this log allows. A batch that deleted anything records an
    /// `audit.purged` entry itself; a `cutoff` younger than
    /// [`AUDIT_RETENTION_FLOOR`] is refused rather than clamped, because a caller
    /// asking for that is a bug or an attack and neither should succeed quietly.
    async fn purge_older_than(&self, cutoff: OffsetDateTime, batch: u32) -> PortResult<u64>;
}
